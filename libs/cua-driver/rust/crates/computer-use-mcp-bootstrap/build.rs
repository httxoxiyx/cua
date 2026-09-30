fn main() {
    // Release builders stamp these values into the signed bootstrap so a
    // downstream packager can prove which reviewed source produced it.
    println!("cargo:rerun-if-env-changed=CUA_DRIVER_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=CUA_DRIVER_SOURCE_SHA");
}
