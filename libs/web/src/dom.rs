//! Basic DOM data structures (from robinson).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

pub type AttrMap = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub struct Node {
    pub children: Vec<Node>,
    pub node_type: NodeType,
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
    Node { children: Vec::new(), node_type: NodeType::Text(data) }
}

pub fn elem(tag_name: String, attrs: AttrMap, children: Vec<Node>) -> Node {
    Node { children, node_type: NodeType::Element(ElementData { tag_name, attrs }) }
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
