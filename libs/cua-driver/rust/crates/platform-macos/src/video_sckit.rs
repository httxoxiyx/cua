//! Native ScreenCaptureKit video backend (macOS).
//!
//! Replaces the ffmpeg subprocess pipeline with an in-process SCStream +
//! SCRecordingOutput. The key win is TCC: ScreenCaptureKit runs in the
//! same process as cua-driver, so it inherits the daemon's Screen
//! Recording grant. No per-binary subprocess gotcha, no second prompt,
//! no fast-fail-on-hang heuristic.
//!
//! Requires macOS 15.0+ (SCRecordingOutput introduced in macOS 15). The
//! Swift impl this is modelled on lives at
//! `libs/cua-driver/swift/Sources/CuaDriverCore/Recording/VideoRecorder.swift`,
//! though that version composes SCStream + AVAssetWriter manually so it
//! also runs on macOS 14. We use SCRecordingOutput here because the
//! Rust binding doesn't expose AVAssetWriter and macOS 15 is already
//! widespread enough that requiring it is acceptable for the Rust port.
//!
//! Lifecycle:
//!   1. `start(path)` resolves the main display, builds a 30fps full-display
//!      SCStream config + SCRecordingOutput pointing at the mp4 path,
//!      attaches the recording output, calls `start_capture()`.
//!   2. Caller stays alive while recording.
//!   3. `stop()` normally removes the recording output, waits for
//!      `recordingOutputDidFinishRecording`, and only then stops the stream.
//!      If removal fails, stopping the stream triggers the fallback finalization
//!      path and we still wait for the delegate. The callback, not
//!      `stop_capture()` returning, is the point at which ScreenCaptureKit says
//!      the mp4 has finished writing.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use cua_driver_core::video::{VideoBackend, VideoBackendFactory, VideoMetadata};

use screencapturekit::prelude::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
};
use screencapturekit::recording_output::{
    RecordingCallbacks, SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
    SCRecordingOutputFileType,
};

const RECORDING_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum RecordingCompletion {
    Finished,
    Failed(String),
}

#[derive(Debug, Default)]
struct RecordingFinalization {
    cleanup_warnings: Vec<String>,
}

pub struct SckitVideoBackendFactory;

impl VideoBackendFactory for SckitVideoBackendFactory {
    fn start(&self, output_path: &Path) -> anyhow::Result<Box<dyn VideoBackend>> {
        SckitVideoBackend::start(output_path).map(|b| Box::new(b) as Box<dyn VideoBackend>)
    }
}

pub struct SckitVideoBackend {
    stream: SCStream,
    // SCStream's add_recording_output is non-owning — Apple's API requires
    // the SCRecordingOutput stay alive until its delegate reports final
    // completion, so we keep it parked here through teardown.
    recording: SCRecordingOutput,
    completion_rx: Receiver<RecordingCompletion>,
    output_path: std::path::PathBuf,
    started_at: Instant,
}

impl SckitVideoBackend {
    fn start(output_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "failed to create recording output directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        // SCRecordingOutput appends-or-fails on an existing file; match the
        // Swift impl by clearing any stale recording.mp4 from a prior run.
        match std::fs::remove_file(output_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                anyhow::bail!(
                    "failed to remove stale recording file {}: {e}",
                    output_path.display()
                );
            }
        }

        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("SCShareableContent::get failed: {e}"))?;
        let displays = content.displays();
        let display = displays
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no displays available for ScreenCaptureKit"))?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // Match the Swift recorder's pixel resolution + 30fps target. The
        // display's reported width/height are in pixels (already
        // backing-scale-multiplied) on SCDisplay, so passing them through
        // gives a native-resolution capture.
        let pixel_width = display.width();
        let pixel_height = display.height();
        let frame_interval = screencapturekit::cm::CMTime::new(1, 30);
        let config = SCStreamConfiguration::new()
            .with_width(pixel_width)
            .with_height(pixel_height)
            .with_minimum_frame_interval(&frame_interval)
            .with_shows_cursor(true);

        let rec_config = SCRecordingOutputConfiguration::new()
            .with_output_url(output_path)
            .with_video_codec(SCRecordingOutputCodec::H264)
            .with_output_file_type(SCRecordingOutputFileType::MP4);

        let (completion_tx, completion_rx) = mpsc::channel();
        let finished_tx = completion_tx.clone();
        let delegate = RecordingCallbacks::new()
            .on_finish(move || {
                let _ = finished_tx.send(RecordingCompletion::Finished);
            })
            .on_fail(move |error| {
                let _ = completion_tx.send(RecordingCompletion::Failed(error));
            });
        let recording =
            SCRecordingOutput::new_with_delegate(&rec_config, delegate).ok_or_else(|| {
                anyhow::anyhow!(
                    "SCRecordingOutput::new returned nil — macOS 15.0+ is required for \
                 native ScreenCaptureKit video; older macOS needs to use the ffmpeg \
                 backend (currently disabled on macOS)."
                )
            })?;

        let stream = SCStream::new(&filter, &config);
        stream
            .add_recording_output(&recording)
            .map_err(|e| anyhow::anyhow!("SCStream::add_recording_output failed: {e}"))?;
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("SCStream::start_capture failed: {e}"))?;

        tracing::info!(
            target: "recording",
            path = %output_path.display(),
            width = pixel_width,
            height = pixel_height,
            "sckit video capture started"
        );

        Ok(Self {
            stream,
            recording,
            completion_rx,
            output_path: output_path.to_path_buf(),
            started_at: Instant::now(),
        })
    }
}

fn finalize_recording_output(
    remove_recording_output: impl FnOnce() -> anyhow::Result<()>,
    stop_capture: impl FnOnce() -> anyhow::Result<()>,
    completion_rx: &Receiver<RecordingCompletion>,
    timeout: Duration,
) -> anyhow::Result<RecordingFinalization> {
    fn wait_for_completion(
        completion_rx: &Receiver<RecordingCompletion>,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        match completion_rx.recv_timeout(timeout) {
            Ok(RecordingCompletion::Finished) => Ok(()),
            Ok(RecordingCompletion::Failed(error)) => Err(anyhow::anyhow!(
                "SCRecordingOutput failed to finalize: {error}"
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                "timed out after {} ms waiting for SCRecordingOutput to finalize",
                timeout.as_millis()
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "SCRecordingOutput finalization delegate disconnected"
            )),
        }
    }

    // Apple's documented teardown starts by removing the recording output.
    // That requests encoder finalization while leaving the stream alive long
    // enough for the delegate to flush and publish the completed file.
    let remove_result = remove_recording_output();
    let (completion_result, stop_result) = match &remove_result {
        // On the documented path, preserve the live stream until the encoder
        // delegate confirms that the file is complete.
        Ok(()) => {
            let completion_result = wait_for_completion(completion_rx, timeout);
            (completion_result, stop_capture())
        }
        // If removal itself fails, stopping the stream is the only remaining
        // way to ask ScreenCaptureKit to finalize its recording output. Do not
        // return (and drop the delegate) until its terminal callback arrives.
        Err(_) => {
            let stop_result = stop_capture();
            let completion_result = wait_for_completion(completion_rx, timeout);
            (completion_result, stop_result)
        }
    };

    completion_result?;

    // A successful delegate callback means the output file is finalized. At
    // that point remove/stop failures are cleanup problems, not reasons to
    // discard valid metadata for the completed recording.
    let mut cleanup_warnings = Vec::new();
    if let Err(error) = remove_result {
        cleanup_warnings.push(error.to_string());
    }
    if let Err(error) = stop_result {
        cleanup_warnings.push(error.to_string());
    }
    Ok(RecordingFinalization { cleanup_warnings })
}

impl VideoBackend for SckitVideoBackend {
    fn stop(self: Box<Self>) -> anyhow::Result<VideoMetadata> {
        let elapsed = self.started_at.elapsed();
        let finalization = finalize_recording_output(
            || {
                self.stream
                    .remove_recording_output(&self.recording)
                    .map_err(|error| {
                        anyhow::anyhow!("SCStream::remove_recording_output failed: {error}")
                    })
            },
            || {
                self.stream
                    .stop_capture()
                    .map_err(|error| anyhow::anyhow!("SCStream::stop_capture failed: {error}"))
            },
            &self.completion_rx,
            RECORDING_FINALIZATION_TIMEOUT,
        )?;
        for warning in finalization.cleanup_warnings {
            tracing::warn!(
                target: "recording",
                error = %warning,
                "ScreenCaptureKit recording finalized with a cleanup warning"
            );
        }
        Ok(VideoMetadata {
            path: self.output_path,
            duration_ms: elapsed.as_millis() as u64,
            finalized: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn finalization_waits_for_recording_delegate_before_stopping_stream() {
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let notifier_calls = Arc::clone(&calls);
        let remove_calls = Arc::clone(&calls);
        let stop_calls = Arc::clone(&calls);

        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            notifier_calls.lock().unwrap().push("finish");
            completion_tx
                .send(RecordingCompletion::Finished)
                .expect("completion receiver stays alive");
        });
        let started = Instant::now();
        finalize_recording_output(
            || {
                remove_calls.lock().unwrap().push("remove");
                Ok(())
            },
            || {
                stop_calls.lock().unwrap().push("stop");
                Ok(())
            },
            &completion_rx,
            Duration::from_secs(1),
        )
        .expect("delegate completion should finalize recording");
        notifier.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(35));
        assert_eq!(*calls.lock().unwrap(), ["remove", "finish", "stop"]);
    }

    #[test]
    fn finalization_surfaces_delegate_failure_after_cleanup() {
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        completion_tx
            .send(RecordingCompletion::Failed("encoder failed".into()))
            .unwrap();
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped_for_call = Arc::clone(&stopped);

        let error = finalize_recording_output(
            || Ok(()),
            || {
                stopped_for_call.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &completion_rx,
            Duration::from_secs(1),
        )
        .expect_err("delegate failure must reach the caller");

        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error.to_string().contains("encoder failed"));
    }

    #[test]
    fn remove_failure_stops_stream_then_waits_for_delegate_completion() {
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let remove_calls = Arc::clone(&calls);
        let stop_calls = Arc::clone(&calls);

        let finalization = finalize_recording_output(
            || {
                remove_calls.lock().unwrap().push("remove");
                anyhow::bail!("remove failed")
            },
            || {
                stop_calls.lock().unwrap().push("stop");
                completion_tx
                    .send(RecordingCompletion::Finished)
                    .expect("completion receiver stays alive");
                Ok(())
            },
            &completion_rx,
            Duration::from_secs(1),
        )
        .expect("delegate completion keeps the finalized recording usable");

        assert_eq!(*calls.lock().unwrap(), ["remove", "stop"]);
        assert_eq!(finalization.cleanup_warnings.len(), 1);
        assert!(finalization.cleanup_warnings[0].contains("remove failed"));
    }

    #[test]
    fn remove_failure_fallback_surfaces_delegate_failure_after_stopping() {
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped_for_call = Arc::clone(&stopped);

        let error = finalize_recording_output(
            || anyhow::bail!("remove failed"),
            || {
                stopped_for_call.store(true, std::sync::atomic::Ordering::SeqCst);
                completion_tx
                    .send(RecordingCompletion::Failed("encoder failed".into()))
                    .expect("completion receiver stays alive");
                Ok(())
            },
            &completion_rx,
            Duration::from_secs(1),
        )
        .expect_err("fallback delegate failure must reach the caller");

        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error.to_string().contains("encoder failed"));
    }

    #[test]
    fn finalized_recording_survives_stop_capture_cleanup_failure() {
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        completion_tx.send(RecordingCompletion::Finished).unwrap();

        let finalization = finalize_recording_output(
            || Ok(()),
            || anyhow::bail!("stop failed"),
            &completion_rx,
            Duration::from_secs(1),
        )
        .expect("delegate completion is authoritative for file finalization");

        assert_eq!(finalization.cleanup_warnings.len(), 1);
        assert!(finalization.cleanup_warnings[0].contains("stop failed"));
    }

    #[test]
    fn finalization_timeout_still_stops_stream() {
        let (_completion_tx, completion_rx) = mpsc::sync_channel(1);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped_for_call = Arc::clone(&stopped);

        let error = finalize_recording_output(
            || Ok(()),
            || {
                stopped_for_call.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &completion_rx,
            Duration::from_millis(1),
        )
        .expect_err("missing delegate completion must time out");

        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error.to_string().contains("timed out"));
    }
}
