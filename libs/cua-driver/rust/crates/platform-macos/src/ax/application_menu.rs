//! Read-only proof for an observed application-menu element. App-menu ancestry
//! is deliberately separate from exact document/sheet window ancestry.

use super::bindings::{
    ax_get_window_id, copy_element_attr, copy_string_attr, element_screen_rect, kAXErrorSuccess,
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementGetPid,
    AXUIElementGetTypeID, AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use core_foundation::{
    array::CFArray,
    base::{CFEqual, CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};

const MAX_MENU_DEPTH: usize = 40;
const MAX_MENU_CHILDREN: usize = 1024;
// Apple's public CGWindowLevel.h: kCGPopUpMenuWindowLevel. Not every
// accessory-layer window is an application menu (palettes commonly use 3).
const POPUP_MENU_WINDOW_LEVEL: i32 = 101;
const MAX_MENU_IMAGE_BINDINGS: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct ApplicationMenuImage {
    pub pid: i32,
    pub document_window_id: u32,
    pub menu_window_id: u32,
    pub document_bounds: crate::windows::WindowBounds,
    pub menu_bounds: crate::windows::WindowBounds,
}

trait MenuVisualTree: MenuTree {
    fn children(&self, node: &Self::Node) -> Option<Vec<Self::Node>>;
    fn frame(&self, node: &Self::Node) -> Option<[f64; 4]>;
}

fn select_menu_image<T: MenuVisualTree>(
    tree: &T,
    pid: i32,
    document_window_id: u32,
    windows: &[crate::windows::WindowInfo],
) -> Option<ApplicationMenuImage> {
    if tree.focused_window() != Some(document_window_id) {
        return None;
    }
    let mut documents = windows.iter().filter(|window| {
        window.pid == pid && window.window_id == document_window_id && window.layer == 0
    });
    let document = documents.next()?;
    if documents.next().is_some() || !valid_frame(&bounds_frame(&document.bounds)) {
        return None;
    }
    let root = tree.menu_bar()?;
    if tree.role(&root).as_deref() != Some("AXMenuBar") || tree.owner(&root) != Some(pid) {
        return None;
    }
    let mut stack = vec![(root, Vec::<T::Node>::new())];
    let mut visited = Vec::new();
    let mut candidates = Vec::new();
    while let Some((node, ancestors)) = stack.pop() {
        if !tree.within_budget()
            || ancestors.len() >= MAX_MENU_DEPTH
            || visited.len() >= MAX_MENU_CHILDREN
            || visited.iter().any(|seen| tree.same(seen, &node))
            || tree.owner(&node) != Some(pid)
        {
            return None;
        }
        visited.push(node.clone());
        match tree.role(&node).as_deref() {
            Some("AXMenu") => {
                // Closed menus normally have no frame. Do not walk their hidden
                // descendants or turn menu-bar backing windows into captures.
                let Some(frame) = tree.frame(&node).filter(valid_frame) else {
                    continue;
                };
                if !proves_menu_path(tree, pid, document_window_id, &node) {
                    return None;
                }
                let mut matches = windows.iter().filter(|window| {
                    window.pid == pid
                        && window.window_id != document_window_id
                        && window.layer == POPUP_MENU_WINDOW_LEVEL
                        && window.is_on_screen
                        && window.on_current_space != Some(false)
                        && frames_match(&frame, &bounds_frame(&window.bounds))
                });
                // Some apps retain old positive AX frames for closed menus.
                // A frame with no current CG match is not visible evidence;
                // keep looking at sibling branches instead of choosing it.
                let Some(window) = matches.next() else {
                    continue;
                };
                if matches.next().is_some() {
                    return None;
                }
                candidates.push((node.clone(), ancestors.clone(), window));
            }
            Some("AXMenuBar") if ancestors.is_empty() => {}
            Some("AXMenuBarItem" | "AXMenuItem") => {}
            // Decorative AX children are not a menu-tree traversal surface.
            _ => continue,
        }
        let children = tree.children(&node)?;
        if children.len() > MAX_MENU_CHILDREN {
            return None;
        }
        let mut path = ancestors;
        path.push(node);
        for child in children {
            stack.push((child, path.clone()));
        }
    }
    let (selected, path, window) = candidates.iter().max_by_key(|(_, path, _)| path.len())?;
    // Multiple open menu branches are ambiguous. A submenu is eligible only
    // when every other visible menu is its proven ancestor, not a peer window.
    if candidates.iter().any(|(node, _, _)| {
        !tree.same(node, selected) && !path.iter().any(|ancestor| tree.same(ancestor, node))
    }) || tree.focused_window() != Some(document_window_id)
    {
        return None;
    }
    Some(ApplicationMenuImage {
        pid,
        document_window_id,
        menu_window_id: window.window_id,
        document_bounds: document.bounds.clone(),
        menu_bounds: window.bounds.clone(),
    })
}

fn bounds_frame(bounds: &crate::windows::WindowBounds) -> [f64; 4] {
    [bounds.x, bounds.y, bounds.width, bounds.height]
}

fn valid_frame(frame: &[f64; 4]) -> bool {
    frame.iter().all(|value| value.is_finite()) && frame[2] > 0.0 && frame[3] > 0.0
}

fn frames_match(left: &[f64; 4], right: &[f64; 4]) -> bool {
    valid_frame(left)
        && valid_frame(right)
        && left.iter().zip(right).all(|(a, b)| (a - b).abs() <= 0.5)
}

pub(crate) fn is_actionable_menu_role(role: &str) -> bool {
    matches!(role, "AXMenuBarItem" | "AXMenuItem")
}

/// Unlike the legacy generic click mapper, an unknown action must never turn
/// into AXPress. Membership authorizes only the requested advertised action.
pub(crate) fn advertised_menu_action(action: &str, advertised: &[String]) -> Option<&'static str> {
    let native = match action.to_lowercase().as_str() {
        "press" | "click" => "AXPress",
        "show_menu" | "right_click" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        _ => return None,
    };
    advertised
        .iter()
        .any(|candidate| candidate == native)
        .then_some(native)
}

trait MenuTree {
    type Node: Clone;
    fn focused_window(&self) -> Option<u32>;
    fn menu_bar(&self) -> Option<Self::Node>;
    fn owner(&self, node: &Self::Node) -> Option<i32>;
    fn role(&self, node: &Self::Node) -> Option<String>;
    fn parent(&self, node: &Self::Node) -> Option<Self::Node>;
    fn contains_child(&self, parent: &Self::Node, child: &Self::Node) -> bool;
    fn same(&self, left: &Self::Node, right: &Self::Node) -> bool;
    fn within_budget(&self) -> bool {
        true
    }
}

fn proves_menu_member<T: MenuTree>(tree: &T, pid: i32, window_id: u32, element: &T::Node) -> bool {
    tree.role(element)
        .is_some_and(|role| is_actionable_menu_role(&role))
        && proves_menu_path(tree, pid, window_id, element)
}

// Shared read-only ancestry proof. AXMenu containers are visual evidence only;
// the semantic action entrypoint above still requires an actionable item role.
fn proves_menu_path<T: MenuTree>(tree: &T, pid: i32, window_id: u32, element: &T::Node) -> bool {
    if tree.focused_window() != Some(window_id) {
        return false;
    }
    let Some(root) = tree.menu_bar() else {
        return false;
    };
    if tree.role(&root).as_deref() != Some("AXMenuBar") || tree.owner(&root) != Some(pid) {
        return false;
    }
    let mut current = element.clone();
    let mut visited = Vec::new();
    for _ in 0..MAX_MENU_DEPTH {
        if !tree.within_budget()
            || tree.owner(&current) != Some(pid)
            || visited.iter().any(|node| tree.same(node, &current))
        {
            return false;
        }
        if tree.same(&current, &root) {
            // The application can change document context during AX reads.
            // Re-read it after membership proof; never use global foreground.
            return tree.focused_window() == Some(window_id);
        }
        if !matches!(
            tree.role(&current).as_deref(),
            Some("AXMenuBarItem" | "AXMenuItem" | "AXMenu")
        ) {
            return false;
        }
        let Some(parent) = tree.parent(&current) else {
            return false;
        };
        // Parent links alone can outlive a replaced/opened menu. Require the
        // fresh parent's current children to contain this exact AX identity.
        if !tree.contains_child(&parent, &current) {
            return false;
        }
        visited.push(current);
        current = parent;
    }
    false
}

struct AxNode(AXUIElementRef);
impl AxNode {
    unsafe fn owned(ptr: AXUIElementRef) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(ptr, 0.2);
        Some(Self(ptr))
    }
}
impl Clone for AxNode {
    fn clone(&self) -> Self {
        unsafe {
            CFRetain(self.0 as CFTypeRef);
        }
        Self(self.0)
    }
}
impl Drop for AxNode {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0 as CFTypeRef);
        }
    }
}
struct NativeMenuTree {
    app: AxNode,
    deadline: Option<std::time::Instant>,
}
impl MenuTree for NativeMenuTree {
    type Node = AxNode;
    fn focused_window(&self) -> Option<u32> {
        unsafe {
            let window = AxNode::owned(copy_element_attr(self.app.0, "AXFocusedWindow")?)?;
            ax_get_window_id(window.0)
        }
    }
    fn menu_bar(&self) -> Option<AxNode> {
        unsafe { AxNode::owned(copy_element_attr(self.app.0, "AXMenuBar")?) }
    }
    fn owner(&self, node: &AxNode) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(node.0, &mut pid) } == kAXErrorSuccess).then_some(pid)
    }
    fn role(&self, node: &AxNode) -> Option<String> {
        unsafe { copy_string_attr(node.0, "AXRole") }
    }
    fn parent(&self, node: &AxNode) -> Option<AxNode> {
        unsafe { AxNode::owned(copy_element_attr(node.0, "AXParent")?) }
    }
    fn contains_child(&self, parent: &AxNode, child: &AxNode) -> bool {
        unsafe {
            let attribute = CFString::new("AXChildren");
            let mut value: CFTypeRef = std::ptr::null();
            let error = AXUIElementCopyAttributeValue(
                parent.0,
                attribute.as_concrete_TypeRef(),
                &mut value,
            );
            if error != kAXErrorSuccess || value.is_null() {
                return false;
            }
            if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
                CFRelease(value);
                return false;
            }
            let children = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
            children.len() <= MAX_MENU_CHILDREN as isize
                && (0..children.len()).any(|index| {
                    CFEqual(
                        *children.get(index).expect("bounded CFArray index"),
                        child.0 as CFTypeRef,
                    ) != 0
                })
        }
    }
    fn same(&self, left: &AxNode, right: &AxNode) -> bool {
        unsafe { CFEqual(left.0 as CFTypeRef, right.0 as CFTypeRef) != 0 }
    }
    fn within_budget(&self) -> bool {
        self.deadline
            .is_none_or(|deadline| std::time::Instant::now() < deadline)
    }
}

impl MenuVisualTree for NativeMenuTree {
    fn children(&self, node: &AxNode) -> Option<Vec<AxNode>> {
        unsafe {
            let attribute = CFString::new("AXChildren");
            let mut value: CFTypeRef = std::ptr::null();
            let error =
                AXUIElementCopyAttributeValue(node.0, attribute.as_concrete_TypeRef(), &mut value);
            if error != kAXErrorSuccess || value.is_null() {
                // Leaf menu commands need not expose AXChildren.
                return (self.role(node).as_deref() == Some("AXMenuItem")).then(Vec::new);
            }
            if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
                CFRelease(value);
                return None;
            }
            let children = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
            if children.len() > MAX_MENU_CHILDREN as isize {
                return None;
            }
            let mut result = Vec::new();
            for index in 0..children.len() {
                let child = *children.get(index)?;
                if CFGetTypeID(child) != AXUIElementGetTypeID() {
                    return None;
                }
                CFRetain(child);
                result.push(AxNode::owned(child as AXUIElementRef)?);
            }
            Some(result)
        }
    }
    fn frame(&self, node: &AxNode) -> Option<[f64; 4]> {
        unsafe { element_screen_rect(node.0) }
    }
}

/// Select one currently visible application menu, never an arbitrary same-app
/// palette or unrelated document. No application activation or AX action.
pub(crate) fn active_application_menu(
    pid: i32,
    document_window_id: u32,
) -> Option<ApplicationMenuImage> {
    let enumeration = crate::windows::all_windows_including_accessory_layers_with_snapshot();
    if !enumeration.succeeded {
        return None;
    }
    // A successful all-window snapshot can retire closed/re-owned document
    // anchors. Merely closing the menu or aging a record never restores pixel
    // authority for a still-live document.
    observed_menu_images()
        .lock()
        .unwrap()
        .retain(|(owner, document), _| {
            enumeration
                .windows
                .iter()
                .any(|window| window.pid == *owner && window.window_id == *document)
        });
    // Ordinary observations do no AX menu traversal when WindowServer has no
    // same-process on-screen popup-menu surface to prove.
    if !enumeration.windows.iter().any(|window| {
        window.pid == pid
            && window.layer == POPUP_MENU_WINDOW_LEVEL
            && window.is_on_screen
            && window.on_current_space != Some(false)
    }) {
        return None;
    }
    let app = unsafe { AxNode::owned(AXUIElementCreateApplication(pid)) }?;
    let tree = NativeMenuTree {
        app,
        deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(2)),
    };
    select_menu_image(&tree, pid, document_window_id, &enumeration.windows)
}

pub(crate) fn revalidate_menu_image(image: &ApplicationMenuImage) -> bool {
    active_application_menu(image.pid, image.document_window_id).is_some_and(|current| {
        current.menu_window_id == image.menu_window_id
            && frames_match(
                &bounds_frame(&current.menu_bounds),
                &bounds_frame(&image.menu_bounds),
            )
            && frames_match(
                &bounds_frame(&current.document_bounds),
                &bounds_frame(&image.document_bounds),
            )
    })
}

// This is an observation-scope guard, not a new pointer route. A tree-only
// refresh must not make pixels from the prior menu image document-relative.
fn observed_menu_images(
) -> &'static std::sync::Mutex<std::collections::HashMap<(i32, u32), ApplicationMenuImage>> {
    static IMAGES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(i32, u32), ApplicationMenuImage>>,
    > = std::sync::OnceLock::new();
    IMAGES.get_or_init(Default::default)
}

pub(crate) fn remember_observed_image(
    pid: i32,
    document_window_id: u32,
    menu: Option<&ApplicationMenuImage>,
) -> bool {
    let mut images = observed_menu_images().lock().unwrap();
    if let Some(menu) = menu {
        if !images.contains_key(&(pid, document_window_id))
            && images.len() >= MAX_MENU_IMAGE_BINDINGS
        {
            return false;
        }
        images.insert((pid, document_window_id), menu.clone());
    } else {
        images.remove(&(pid, document_window_id));
    }
    true
}

pub(crate) fn menu_image_coordinates_withheld(window_id: u32) -> bool {
    observed_menu_images()
        .lock()
        .unwrap()
        .values()
        .any(|image| image.document_window_id == window_id || image.menu_window_id == window_id)
}

/// PiP gets no independent app-wide selection authority. It must still refer
/// to the recorded observation and re-prove that exact menu before display.
pub(crate) fn observed_live_menu_image(
    pid: i32,
    menu_window_id: u32,
) -> Option<ApplicationMenuImage> {
    let image = observed_menu_images()
        .lock()
        .unwrap()
        .values()
        .find(|image| image.pid == pid && image.menu_window_id == menu_window_id)
        .cloned()?;
    revalidate_menu_image(&image).then_some(image)
}

/// Prove current membership under the requested process's application menu
/// and its exact app-local document context. Does not activate the application
/// or grant document-window ancestry to menu objects.
///
/// # Safety
/// `element` must remain retained for the duration of this read-only proof.
pub(crate) unsafe fn proves_application_menu(
    pid: i32,
    window_id: u32,
    element: AXUIElementRef,
) -> bool {
    let Some(app) = AxNode::owned(AXUIElementCreateApplication(pid)) else {
        return false;
    };
    if element.is_null() {
        return false;
    }
    CFRetain(element as CFTypeRef);
    let element = AxNode::owned(element).expect("non-null retained element");
    proves_menu_member(
        &NativeMenuTree {
            app,
            deadline: None,
        },
        pid,
        window_id,
        &element,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    #[test]
    fn menu_actions_require_an_exact_supported_advertisement() {
        for (action, native) in [
            ("press", "AXPress"),
            ("click", "AXPress"),
            ("show_menu", "AXShowMenu"),
            ("right_click", "AXShowMenu"),
            ("pick", "AXPick"),
            ("confirm", "AXConfirm"),
            ("cancel", "AXCancel"),
            ("open", "AXOpen"),
        ] {
            assert_eq!(
                advertised_menu_action(action, &[native.into()]),
                Some(native)
            );
            assert_eq!(advertised_menu_action(action, &[]), None);
        }
        assert_eq!(advertised_menu_action("typo", &["AXPress".into()]), None);
        assert_eq!(
            advertised_menu_action("press", &["AXShowMenu".into()]),
            None
        );
        assert_eq!(
            advertised_menu_action("set_value", &["set_value".into()]),
            None
        );
    }

    #[derive(Clone)]
    struct Node {
        identity: u32,
        role: &'static str,
        owner: i32,
        parent: Option<u32>,
        children: Vec<u32>,
    }
    struct Tree {
        nodes: HashMap<u32, Node>,
        focused: Option<u32>,
        root: Option<u32>,
        focus_reads: Cell<usize>,
        lose_focus_after_first: bool,
    }
    impl MenuTree for Tree {
        type Node = u32;
        fn focused_window(&self) -> Option<u32> {
            let reads = self.focus_reads.get();
            self.focus_reads.set(reads + 1);
            if self.lose_focus_after_first && reads > 0 {
                None
            } else {
                self.focused
            }
        }
        fn menu_bar(&self) -> Option<u32> {
            self.root
        }
        fn owner(&self, node: &u32) -> Option<i32> {
            self.nodes.get(node).map(|n| n.owner)
        }
        fn role(&self, node: &u32) -> Option<String> {
            self.nodes.get(node).map(|n| n.role.to_owned())
        }
        fn parent(&self, node: &u32) -> Option<u32> {
            self.nodes.get(node)?.parent
        }
        fn contains_child(&self, parent: &u32, child: &u32) -> bool {
            self.nodes
                .get(parent)
                .is_some_and(|n| n.children.iter().any(|id| self.same(id, child)))
        }
        fn same(&self, left: &u32, right: &u32) -> bool {
            self.nodes
                .get(left)
                .zip(self.nodes.get(right))
                .is_some_and(|(l, r)| l.identity == r.identity)
        }
    }
    fn menu_tree() -> Tree {
        Tree {
            focused: Some(7),
            root: Some(0),
            focus_reads: Cell::new(0),
            lose_focus_after_first: false,
            nodes: [
                (
                    0,
                    Node {
                        identity: 0,
                        role: "AXMenuBar",
                        owner: 42,
                        parent: None,
                        children: vec![1],
                    },
                ),
                (
                    1,
                    Node {
                        identity: 1,
                        role: "AXMenuBarItem",
                        owner: 42,
                        parent: Some(0),
                        children: vec![2],
                    },
                ),
                (
                    2,
                    Node {
                        identity: 2,
                        role: "AXMenu",
                        owner: 42,
                        parent: Some(1),
                        children: vec![3],
                    },
                ),
                (
                    3,
                    Node {
                        identity: 3,
                        role: "AXMenuItem",
                        owner: 42,
                        parent: Some(2),
                        children: vec![],
                    },
                ),
            ]
            .into(),
        }
    }

    struct VisualTree {
        tree: Tree,
        frames: HashMap<u32, [f64; 4]>,
    }
    impl MenuTree for VisualTree {
        type Node = u32;
        fn focused_window(&self) -> Option<u32> {
            self.tree.focused_window()
        }
        fn menu_bar(&self) -> Option<u32> {
            self.tree.menu_bar()
        }
        fn owner(&self, node: &u32) -> Option<i32> {
            self.tree.owner(node)
        }
        fn role(&self, node: &u32) -> Option<String> {
            self.tree.role(node)
        }
        fn parent(&self, node: &u32) -> Option<u32> {
            self.tree.parent(node)
        }
        fn contains_child(&self, parent: &u32, child: &u32) -> bool {
            self.tree.contains_child(parent, child)
        }
        fn same(&self, left: &u32, right: &u32) -> bool {
            self.tree.same(left, right)
        }
    }
    impl MenuVisualTree for VisualTree {
        fn children(&self, node: &u32) -> Option<Vec<u32>> {
            Some(self.tree.nodes.get(node)?.children.clone())
        }
        fn frame(&self, node: &u32) -> Option<[f64; 4]> {
            self.frames.get(node).copied()
        }
    }
    fn visual_tree() -> VisualTree {
        VisualTree {
            tree: menu_tree(),
            frames: [(2, [215.0, 34.0, 203.0, 117.0])].into(),
        }
    }
    fn visual_window(id: u32, pid: i32, layer: i32, frame: [f64; 4]) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id: id,
            pid,
            app_name: "Fixture".into(),
            title: String::new(),
            bounds: crate::windows::WindowBounds {
                x: frame[0],
                y: frame[1],
                width: frame[2],
                height: frame[3],
            },
            layer,
            z_index: 0,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }
    fn visual_windows() -> Vec<crate::windows::WindowInfo> {
        vec![
            visual_window(7, 42, 0, [504.0, 63.0, 700.0, 892.0]),
            visual_window(9, 42, 101, [215.0, 34.0, 203.0, 117.0]),
        ]
    }
    #[test]
    fn menu_image_keeps_document_anchor_and_selects_only_proven_popup() {
        let image = select_menu_image(&visual_tree(), 42, 7, &visual_windows())
            .expect("the current AX menu has an exact same-owner CG frame");
        assert_eq!(
            (image.pid, image.document_window_id, image.menu_window_id),
            (42, 7, 9)
        );
        assert_eq!(image.document_bounds.x, 504.0);
        assert_eq!(image.menu_bounds.x, 215.0);
        // Read-only AXMenu membership must not widen semantic-action eligibility.
        assert!(!proves_menu_member(&visual_tree(), 42, 7, &2));
    }
    #[test]
    fn menu_image_rejects_foreign_hidden_zero_layer_and_mismatched_frames() {
        for mutation in 0..6 {
            let mut windows = visual_windows();
            match mutation {
                0 => windows[1].pid = 99,
                1 => windows[1].is_on_screen = false,
                2 => windows[1].layer = 0,
                3 => windows[1].bounds.x += 1.0,
                4 => windows[1].bounds.width = f64::NAN,
                _ => windows[0].pid = 99,
            }
            assert!(
                select_menu_image(&visual_tree(), 42, 7, &windows).is_none(),
                "mutation {mutation}"
            );
        }
    }
    #[test]
    fn menu_image_rejects_ambiguous_window_match() {
        let mut windows = visual_windows();
        let mut duplicate = windows[1].clone();
        duplicate.window_id = 10;
        windows.push(duplicate);
        assert!(select_menu_image(&visual_tree(), 42, 7, &windows).is_none());
    }
    #[test]
    fn menu_image_rejects_stale_focus_or_detached_ancestry() {
        for mutation in 0..5 {
            let mut tree = visual_tree();
            match mutation {
                0 => tree.tree.focused = Some(8),
                1 => tree.tree.lose_focus_after_first = true,
                2 => tree.tree.nodes.get_mut(&1).unwrap().children.clear(),
                3 => tree.tree.nodes.get_mut(&2).unwrap().owner = 99,
                _ => tree.tree.nodes.get_mut(&2).unwrap().parent = Some(2),
            }
            assert!(
                select_menu_image(&tree, 42, 7, &visual_windows()).is_none(),
                "mutation {mutation}"
            );
        }
    }
    #[test]
    fn menu_image_closed_unframed_menu_is_not_a_visual_target() {
        let mut tree = visual_tree();
        tree.frames.clear();
        assert!(select_menu_image(&tree, 42, 7, &visual_windows()).is_none());
    }
    #[test]
    fn menu_image_prefers_deepest_open_submenu_in_the_same_chain() {
        let mut tree = visual_tree();
        tree.tree.nodes.get_mut(&3).unwrap().children = vec![4];
        tree.tree.nodes.insert(
            4,
            Node {
                identity: 4,
                role: "AXMenu",
                owner: 42,
                parent: Some(3),
                children: vec![],
            },
        );
        tree.frames.insert(4, [418.0, 40.0, 100.0, 60.0]);
        let mut windows = visual_windows();
        windows.push(visual_window(10, 42, 101, [418.0, 40.0, 100.0, 60.0]));
        assert_eq!(
            select_menu_image(&tree, 42, 7, &windows)
                .unwrap()
                .menu_window_id,
            10
        );
    }
    #[test]
    fn menu_image_rejects_two_unrelated_open_menu_branches() {
        let mut tree = visual_tree();
        tree.tree.nodes.get_mut(&0).unwrap().children.push(4);
        tree.tree.nodes.insert(
            4,
            Node {
                identity: 4,
                role: "AXMenuBarItem",
                owner: 42,
                parent: Some(0),
                children: vec![5],
            },
        );
        tree.tree.nodes.insert(
            5,
            Node {
                identity: 5,
                role: "AXMenu",
                owner: 42,
                parent: Some(4),
                children: vec![],
            },
        );
        tree.frames.insert(5, [418.0, 40.0, 100.0, 60.0]);
        let mut windows = visual_windows();
        windows.push(visual_window(10, 42, 101, [418.0, 40.0, 100.0, 60.0]));
        assert!(select_menu_image(&tree, 42, 7, &windows).is_none());
    }

    #[test]
    fn menu_image_ignores_closed_sibling_with_stale_positive_ax_frame() {
        let mut tree = visual_tree();
        tree.tree.nodes.get_mut(&0).unwrap().children.push(4);
        tree.tree.nodes.insert(
            4,
            Node {
                identity: 4,
                role: "AXMenuBarItem",
                owner: 42,
                parent: Some(0),
                children: vec![5],
            },
        );
        tree.tree.nodes.insert(
            5,
            Node {
                identity: 5,
                role: "AXMenu",
                owner: 42,
                parent: Some(4),
                children: vec![],
            },
        );
        tree.frames.insert(5, [800.0, 40.0, 100.0, 60.0]);
        assert_eq!(
            select_menu_image(&tree, 42, 7, &visual_windows())
                .unwrap()
                .menu_window_id,
            9
        );
    }

    #[test]
    fn menu_image_pixel_scope_survives_until_an_ordinary_image_replaces_it() {
        let mut image = select_menu_image(&visual_tree(), 42, 7, &visual_windows()).unwrap();
        // Distinct ids keep this global-registry test independent of fixtures.
        image.document_window_id = 900_007;
        image.menu_window_id = 900_009;
        remember_observed_image(42, image.document_window_id, Some(&image));
        assert!(menu_image_coordinates_withheld(image.document_window_id));
        assert!(menu_image_coordinates_withheld(image.menu_window_id));
        assert!(!menu_image_coordinates_withheld(900_008));
        // A different window's ordinary image cannot clear this document's
        // scope. Tree-only observations never call remember_observed_image.
        remember_observed_image(42, 900_008, None);
        assert!(menu_image_coordinates_withheld(image.document_window_id));
        remember_observed_image(42, image.document_window_id, None);
        assert!(!menu_image_coordinates_withheld(image.document_window_id));
        assert!(!menu_image_coordinates_withheld(image.menu_window_id));
    }
    #[test]
    fn native_menu_proof_accepts_live_menu_bar_and_nested_item() {
        let tree = menu_tree();
        assert!(proves_menu_member(&tree, 42, 7, &1));
        assert!(proves_menu_member(&tree, 42, 7, &3));
    }
    #[test]
    fn native_menu_proof_compares_identity_not_proxy_address() {
        let mut tree = menu_tree();
        tree.nodes.insert(9, tree.nodes[&3].clone());
        assert!(proves_menu_member(&tree, 42, 7, &9));
    }
    #[test]
    fn native_menu_proof_rejects_wrong_or_unknown_focused_window() {
        let mut tree = menu_tree();
        for focused in [Some(8), None] {
            tree.focused = focused;
            assert!(!proves_menu_member(&tree, 42, 7, &3));
        }
    }

    #[test]
    fn native_menu_proof_rechecks_context_after_traversal() {
        let mut tree = menu_tree();
        tree.lose_focus_after_first = true;
        assert!(!proves_menu_member(&tree, 42, 7, &3));
        assert_eq!(tree.focus_reads.get(), 2);
    }

    #[test]
    fn native_menu_proof_rejects_replaced_root_and_excessive_depth() {
        let mut tree = menu_tree();
        let mut replacement = tree.nodes[&0].clone();
        replacement.identity = 10;
        tree.nodes.insert(10, replacement);
        tree.root = Some(10);
        assert!(!proves_menu_member(&tree, 42, 7, &3));

        let mut tree = menu_tree();
        let last = MAX_MENU_DEPTH as u32 + 4;
        tree.nodes.get_mut(&3).unwrap().parent = Some(4);
        tree.nodes.get_mut(&1).unwrap().children = vec![last];
        for id in 4..=last {
            tree.nodes.insert(
                id,
                Node {
                    identity: id,
                    role: "AXMenu",
                    owner: 42,
                    parent: Some(if id == last { 1 } else { id + 1 }),
                    children: vec![id - 1],
                },
            );
        }
        assert!(!proves_menu_member(&tree, 42, 7, &3));
    }
    #[test]
    fn native_menu_proof_rejects_foreign_detached_and_non_menu_nodes() {
        for node_id in 0..=3 {
            let mut tree = menu_tree();
            tree.nodes.get_mut(&node_id).unwrap().owner = 99;
            assert!(!proves_menu_member(&tree, 42, 7, &3));
        }
        let mut tree = menu_tree();
        tree.nodes.get_mut(&2).unwrap().children.clear();
        assert!(!proves_menu_member(&tree, 42, 7, &3));
        for role in [
            "AXWindow",
            "AXSheet",
            "AXApplication",
            "AXWebArea",
            "AXGroup",
        ] {
            let mut tree = menu_tree();
            tree.nodes.get_mut(&2).unwrap().role = role;
            assert!(!proves_menu_member(&tree, 42, 7, &3));
        }
        assert!(!proves_menu_member(&menu_tree(), 42, 7, &0));
        assert!(!proves_menu_member(&menu_tree(), 42, 7, &2));
    }
    #[test]
    fn native_menu_proof_rejects_missing_root_and_cycles() {
        let mut tree = menu_tree();
        tree.root = None;
        assert!(!proves_menu_member(&tree, 42, 7, &3));
        tree.root = Some(0);
        tree.nodes.get_mut(&1).unwrap().parent = Some(2);
        tree.nodes.get_mut(&2).unwrap().children.push(1);
        assert!(!proves_menu_member(&tree, 42, 7, &3));
    }
}
