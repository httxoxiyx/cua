//! Semantic-only proof for a control in an AXPopover attached to one host window.
//!
//! Pages color swatches name the popover (a distinct CGWindowID) as AXWindow,
//! while the popover's reciprocal AXParent chain and AXWindow name the document.
//! Keep that physical window identity intact for pointer and keyboard routing.

use super::bindings::{
    ax_get_window_id, copy_element_attr, copy_string_attr, kAXErrorSuccess,
    AXUIElementCopyAttributeValue, AXUIElementGetPid, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use core_foundation::{
    array::CFArray,
    base::{CFEqual, CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};
use std::time::{Duration, Instant};

const MAX_DEPTH: usize = 32;
const MAX_CHILDREN: isize = 1024;

trait PopoverTree {
    type Node: Clone;
    fn role(&self, node: &Self::Node) -> Option<String>;
    fn owner(&self, node: &Self::Node) -> Option<i32>;
    fn window(&self, node: &Self::Node) -> Option<Self::Node>;
    fn window_id(&self, node: &Self::Node) -> Option<u32>;
    fn parent(&self, node: &Self::Node) -> Option<Self::Node>;
    fn contains_child(&self, parent: &Self::Node, child: &Self::Node) -> bool;
    fn same(&self, left: &Self::Node, right: &Self::Node) -> bool;
    fn within_budget(&self) -> bool;
}

fn prove<T: PopoverTree>(tree: &T, pid: i32, host_id: u32, element: &T::Node) -> bool {
    // This route deliberately covers controls, not text/value writes or focus.
    if tree.role(element).as_deref() != Some("AXButton") {
        return false;
    }
    let Some(popover) = tree.window(element) else {
        return false;
    };
    if tree.role(&popover).as_deref() != Some("AXPopover") || tree.owner(&popover) != Some(pid) {
        return false;
    }
    let Some(popover_id) = tree.window_id(&popover).filter(|id| *id != host_id) else {
        return false;
    };
    let Some(host) = tree.window(&popover) else {
        return false;
    };
    if !matches!(tree.role(&host).as_deref(), Some("AXWindow" | "AXSheet"))
        || tree.owner(&host) != Some(pid)
        || tree.window_id(&host) != Some(host_id)
    {
        return false;
    }

    let mut current = element.clone();
    let mut visited = Vec::new();
    let mut crossed_popover = false;
    for _ in 0..MAX_DEPTH {
        if !tree.within_budget()
            || tree.owner(&current) != Some(pid)
            || visited.iter().any(|node| tree.same(node, &current))
        {
            return false;
        }
        if tree.same(&current, &host) {
            // Re-read the attachment after traversal; an old AXParent alone
            // must not authorize a closed or reattached panel.
            return crossed_popover
                && tree.window_id(&current) == Some(host_id)
                && tree.window_id(&popover) == Some(popover_id)
                && tree
                    .window(element)
                    .is_some_and(|node| tree.same(&node, &popover))
                && tree
                    .window(&popover)
                    .is_some_and(|node| tree.same(&node, &host))
                && tree.within_budget();
        }
        match tree.role(&current).as_deref() {
            Some("AXPopover") if tree.same(&current, &popover) && !crossed_popover => {
                crossed_popover = true;
            }
            Some("AXButton" | "AXRadioGroup" | "AXGroup" | "AXScrollArea" | "AXSplitGroup") => {}
            // A different top-level window, nested popover, web subtree or
            // application root cannot be treated as an attachment to this host.
            _ => return false,
        }
        let Some(parent) = tree.parent(&current) else {
            return false;
        };
        if !tree.contains_child(&parent, &current) {
            return false;
        }
        visited.push(current);
        current = parent;
    }
    false
}

/// No unknown-action mapping, selection writes, ancestor or pixel fallback.
pub(crate) fn advertised_action(action: &str, advertised: &[String]) -> Option<&'static str> {
    let native = match action {
        "press" | "click" => "AXPress",
        "pick" => "AXPick",
        "show_menu" => "AXShowMenu",
        _ => return None,
    };
    advertised
        .iter()
        .any(|value| value == native)
        .then_some(native)
}

struct AxNode(AXUIElementRef);
impl AxNode {
    unsafe fn owned(ptr: AXUIElementRef) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        if AXUIElementSetMessagingTimeout(ptr, 0.2) != kAXErrorSuccess {
            CFRelease(ptr as CFTypeRef);
            return None;
        }
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

struct NativeTree {
    deadline: Instant,
}
impl PopoverTree for NativeTree {
    type Node = AxNode;
    fn role(&self, node: &AxNode) -> Option<String> {
        unsafe { copy_string_attr(node.0, "AXRole") }
    }
    fn owner(&self, node: &AxNode) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(node.0, &mut pid) } == kAXErrorSuccess).then_some(pid)
    }
    fn window(&self, node: &AxNode) -> Option<AxNode> {
        unsafe { AxNode::owned(copy_element_attr(node.0, "AXWindow")?) }
    }
    fn window_id(&self, node: &AxNode) -> Option<u32> {
        unsafe { ax_get_window_id(node.0) }
    }
    fn parent(&self, node: &AxNode) -> Option<AxNode> {
        unsafe { AxNode::owned(copy_element_attr(node.0, "AXParent")?) }
    }
    fn contains_child(&self, parent: &AxNode, child: &AxNode) -> bool {
        unsafe {
            let attr = CFString::new("AXChildren");
            let mut value: CFTypeRef = std::ptr::null();
            if AXUIElementCopyAttributeValue(parent.0, attr.as_concrete_TypeRef(), &mut value)
                != kAXErrorSuccess
                || value.is_null()
            {
                return false;
            }
            if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
                CFRelease(value);
                return false;
            }
            let children = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
            children.len() <= MAX_CHILDREN
                && (0..children.len()).any(|index| {
                    CFEqual(
                        *children.get(index).expect("bounded index"),
                        child.0 as CFTypeRef,
                    ) != 0
                })
        }
    }
    fn same(&self, left: &AxNode, right: &AxNode) -> bool {
        unsafe { CFEqual(left.0 as CFTypeRef, right.0 as CFTypeRef) != 0 }
    }
    fn within_budget(&self) -> bool {
        Instant::now() < self.deadline
    }
}

fn requires_host_attachment(
    role: Option<&str>,
    physical_window: Option<u32>,
    host_id: u32,
) -> bool {
    role == Some("AXPopover") && physical_window.is_some_and(|id| id != host_id)
}

/// Cheap classification only. Exact popover-window targets keep the ordinary
/// route; the full attachment proof is mandatory only for a displaced host.
pub(crate) unsafe fn has_displaced_popover_window(element: AXUIElementRef, host_id: u32) -> bool {
    let Some(window) = copy_element_attr(element, "AXWindow").and_then(|ptr| AxNode::owned(ptr))
    else {
        return false;
    };
    requires_host_attachment(
        copy_string_attr(window.0, "AXRole").as_deref(),
        ax_get_window_id(window.0),
        host_id,
    )
}

/// # Safety
/// `element` must stay retained for this bounded, read-only proof.
pub(crate) unsafe fn proves_attached_popover(
    pid: i32,
    host_id: u32,
    element: AXUIElementRef,
) -> bool {
    if element.is_null() {
        return false;
    }
    CFRetain(element as CFTypeRef);
    let Some(element) = AxNode::owned(element) else {
        return false;
    };
    prove(
        &NativeTree {
            deadline: Instant::now() + Duration::from_secs(2),
        },
        pid,
        host_id,
        &element,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::HashMap};

    #[derive(Clone)]
    struct Node {
        identity: u32,
        role: &'static str,
        owner: i32,
        window: Option<u32>,
        id: Option<u32>,
        parent: Option<u32>,
        children: Vec<u32>,
    }
    struct Tree {
        nodes: HashMap<u32, Node>,
        budget: Cell<usize>,
        reattach: bool,
        popup_window_reads: Cell<usize>,
    }
    impl PopoverTree for Tree {
        type Node = u32;
        fn role(&self, node: &u32) -> Option<String> {
            self.nodes.get(node).map(|n| n.role.into())
        }
        fn owner(&self, node: &u32) -> Option<i32> {
            self.nodes.get(node).map(|n| n.owner)
        }
        fn window(&self, node: &u32) -> Option<u32> {
            if *node == 2 {
                let reads = self.popup_window_reads.get();
                self.popup_window_reads.set(reads + 1);
                if self.reattach && reads > 0 {
                    return Some(8);
                }
            }
            self.nodes.get(node)?.window
        }
        fn window_id(&self, node: &u32) -> Option<u32> {
            self.nodes.get(node)?.id
        }
        fn parent(&self, node: &u32) -> Option<u32> {
            self.nodes.get(node)?.parent
        }
        fn contains_child(&self, parent: &u32, child: &u32) -> bool {
            self.nodes
                .get(parent)
                .is_some_and(|n| n.children.iter().any(|c| self.same(c, child)))
        }
        fn same(&self, a: &u32, b: &u32) -> bool {
            self.nodes
                .get(a)
                .zip(self.nodes.get(b))
                .is_some_and(|(a, b)| a.identity == b.identity)
        }
        fn within_budget(&self) -> bool {
            let remaining = self.budget.get();
            self.budget.set(remaining.saturating_sub(1));
            remaining > 0
        }
    }
    fn pages() -> Tree {
        let roles = [
            "AXButton",
            "AXRadioGroup",
            "AXPopover",
            "AXScrollArea",
            "AXGroup",
            "AXSplitGroup",
            "AXWindow",
        ];
        let nodes = roles
            .into_iter()
            .enumerate()
            .map(|(index, role)| {
                let i = index as u32;
                (
                    i,
                    Node {
                        identity: i,
                        role,
                        owner: 42,
                        window: if i == 0 {
                            Some(2)
                        } else if i < 6 {
                            Some(6)
                        } else {
                            None
                        },
                        id: Some(if i <= 2 { 900 } else { 700 }),
                        parent: (i < 6).then_some(i + 1),
                        children: if i == 0 { vec![] } else { vec![i - 1] },
                    },
                )
            })
            .collect();
        Tree {
            nodes,
            budget: Cell::new(100),
            reattach: false,
            popup_window_reads: Cell::new(0),
        }
    }
    #[test]
    fn pages_swatch_crosses_its_attached_popover_to_exact_host() {
        assert!(prove(&pages(), 42, 700, &0));
    }
    #[test]
    fn exact_popover_window_keeps_ordinary_route() {
        assert!(!requires_host_attachment(Some("AXPopover"), Some(900), 900));
        assert!(requires_host_attachment(Some("AXPopover"), Some(900), 700));
        assert!(!requires_host_attachment(Some("AXPopover"), None, 700));
        assert!(!requires_host_attachment(Some("AXWindow"), Some(900), 700));
    }
    #[test]
    fn equivalent_proxy_identity_is_accepted() {
        let mut tree = pages();
        tree.nodes.insert(9, tree.nodes[&0].clone());
        assert!(prove(&tree, 42, 700, &9));
    }
    #[test]
    fn wrong_host_and_foreign_nodes_are_refused() {
        assert!(!prove(&pages(), 42, 701, &0));
        for i in 0..=6 {
            let mut tree = pages();
            tree.nodes.get_mut(&i).unwrap().owner = 99;
            assert!(!prove(&tree, 42, 700, &0), "foreign node {i}");
        }
    }
    #[test]
    fn detached_and_missing_relations_are_refused() {
        for i in 1..=6 {
            let mut tree = pages();
            tree.nodes.get_mut(&i).unwrap().children.clear();
            assert!(!prove(&tree, 42, 700, &0), "detached node {i}");
        }
        for i in 0..6 {
            let mut tree = pages();
            tree.nodes.get_mut(&i).unwrap().parent = None;
            assert!(!prove(&tree, 42, 700, &0));
        }
        let mut tree = pages();
        tree.nodes.get_mut(&2).unwrap().window = None;
        assert!(!prove(&tree, 42, 700, &0));
    }
    #[test]
    fn cycles_and_deadlines_are_refused() {
        let mut tree = pages();
        tree.nodes.get_mut(&3).unwrap().parent = Some(1);
        tree.nodes.get_mut(&1).unwrap().children.push(3);
        assert!(!prove(&tree, 42, 700, &0));
        for budget in [0, 3, 7] {
            let tree = pages();
            tree.budget.set(budget);
            assert!(!prove(&tree, 42, 700, &0));
        }
    }
    #[test]
    fn excessive_depth_is_refused() {
        let mut tree = pages();
        tree.nodes.get_mut(&3).unwrap().parent = Some(10);
        let last = 10 + MAX_DEPTH as u32;
        tree.nodes.get_mut(&4).unwrap().children = vec![last];
        for i in 10..=last {
            tree.nodes.insert(
                i,
                Node {
                    identity: i,
                    role: "AXGroup",
                    owner: 42,
                    window: Some(6),
                    id: Some(700),
                    parent: Some(if i == last { 4 } else { i + 1 }),
                    children: vec![if i == 10 { 3 } else { i - 1 }],
                },
            );
        }
        assert!(!prove(&tree, 42, 700, &0));
    }
    #[test]
    fn attachment_change_and_non_popover_paths_are_refused() {
        let mut tree = pages();
        tree.reattach = true;
        assert!(!prove(&tree, 42, 700, &0));
        for role in [
            "AXWindow",
            "AXSheet",
            "AXApplication",
            "AXWebArea",
            "AXUnknown",
        ] {
            let mut tree = pages();
            tree.nodes.get_mut(&3).unwrap().role = role;
            assert!(!prove(&tree, 42, 700, &0), "{role}");
        }
        let mut tree = pages();
        tree.nodes.get_mut(&2).unwrap().role = "AXGroup";
        assert!(!prove(&tree, 42, 700, &0));
    }
    #[test]
    fn only_advertised_direct_semantic_actions_are_allowed() {
        assert_eq!(
            advertised_action("press", &["AXPress".into()]),
            Some("AXPress")
        );
        assert_eq!(
            advertised_action("pick", &["AXPick".into()]),
            Some("AXPick")
        );
        for action in ["focus", "confirm", "set_value", "type_text", "unknown"] {
            assert_eq!(advertised_action(action, &["AXPress".into()]), None);
        }
        assert_eq!(advertised_action("press", &["AXShowMenu".into()]), None);
    }
}
