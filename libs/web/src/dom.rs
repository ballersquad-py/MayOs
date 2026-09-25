//! Basic DOM data structures (from robinson).

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};
use alloc::string::String;
use alloc::vec::Vec;

pub type AttrMap = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub struct Node {
    /// Unique for the life of the program (scripts refer to nodes by id).
    pub id: u32,
    pub children: Vec<Node>,
    pub node_type: NodeType,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

pub fn new_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub enum NodeType {
    Element(ElementData),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag_name: String,
    pub attrs: AttrMap,
}

pub fn text(data: String) -> Node {
    Node { id: new_id(), children: Vec::new(), node_type: NodeType::Text(data) }
}

pub fn elem(tag_name: String, attrs: AttrMap, children: Vec<Node>) -> Node {
    Node { id: new_id(), children, node_type: NodeType::Element(ElementData { tag_name, attrs }) }
}

impl ElementData {
    pub fn id(&self) -> Option<&String> {
        self.attrs.get("id")
    }

    pub fn has_class(&self, class: &str) -> bool {
        self.attrs.get("class").map(|c| c.split_ascii_whitespace().any(|x| x == class)).unwrap_or(false)
    }
}

impl Node {
    pub fn element(&self) -> Option<&ElementData> {
        match &self.node_type {
            NodeType::Element(e) => Some(e),
            NodeType::Text(_) => None,
        }
    }

    /// Depth-first search for the first element with this tag.
    pub fn find(&self, tag: &str) -> Option<&Node> {
        if self.element().map(|e| e.tag_name == tag).unwrap_or(false) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(tag))
    }

    /// All text inside this node.
    pub fn text_content(&self) -> String {
        let mut s = String::new();
        self.collect_text(&mut s);
        s
    }

    fn collect_text(&self, out: &mut String) {
        match &self.node_type {
            NodeType::Text(t) => out.push_str(t),
            NodeType::Element(_) => self.children.iter().for_each(|c| c.collect_text(out)),
        }
    }

    pub fn find_id(&self, id: u32) -> Option<&Node> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find_id(id))
    }

    pub fn find_id_mut(&mut self, id: u32) -> Option<&mut Node> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| c.find_id_mut(id))
    }

    /// The parent of node `id` and the node's index in it.
    pub fn parent_of(&self, id: u32) -> Option<(&Node, usize)> {
        for (i, c) in self.children.iter().enumerate() {
            if c.id == id {
                return Some((self, i));
            }
            if let Some(r) = c.parent_of(id) {
                return Some(r);
            }
        }
        None
    }

    /// Remove node `id` from this tree and return it.
    pub fn detach(&mut self, id: u32) -> Option<Node> {
        if let Some(i) = self.children.iter().position(|c| c.id == id) {
            return Some(self.children.remove(i));
        }
        self.children.iter_mut().find_map(|c| c.detach(id))
    }

    /// Serialise the children as HTML.
    pub fn inner_html(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            c.write_html(&mut s);
        }
        s
    }

    pub fn outer_html(&self) -> String {
        let mut s = String::new();
        self.write_html(&mut s);
        s
    }

    fn write_html(&self, out: &mut String) {
        match &self.node_type {
            NodeType::Text(t) => out.push_str(&t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")),
            NodeType::Element(e) => {
                out.push('<');
                out.push_str(&e.tag_name);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&v.replace('&', "&amp;").replace('"', "&quot;"));
                    out.push('"');
                }
                out.push('>');
                if crate::html::is_void(&e.tag_name) {
                    return;
                }
                for c in &self.children {
                    c.write_html(out);
                }
                out.push_str("</");
                out.push_str(&e.tag_name);
                out.push('>');
            }
        }
    }

    /// Visit every element (pre-order).
    pub fn for_each_element<'a>(&'a self, f: &mut dyn FnMut(&'a ElementData, &'a Node)) {
        if let NodeType::Element(e) = &self.node_type {
            f(e, self);
        }
        for c in &self.children {
            c.for_each_element(f);
        }
    }
}
