//! How a machine's page tiles its windows, as in the web UI's layout.js: a
//! binary tree whose leaves are windows and whose splits hold two nodes side
//! by side or stacked, with the share of the space the first one takes.

/// A window on a machine's page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Screen,
    /// A terminal, by its id among the machine's shells.
    Shell(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Side by side.
    Row,
    /// Stacked.
    Column,
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(Pane),
    Split { axis: Axis, ratio: f32, first: Box<Node>, second: Box<Node> },
}

/// A step from a split down to one of its halves.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Step {
    First,
    Second,
}

/// The smallest share a split leaves either of its halves.
const MIN_RATIO: f32 = 0.1;

impl Node {
    pub fn panes(&self) -> Vec<Pane> {
        match self {
            Node::Leaf(pane) => vec![*pane],
            Node::Split { first, second, .. } => [first.panes(), second.panes()].concat(),
        }
    }

    /// `self` without `pane`, its sibling taking the split's place; `None`
    /// when nothing is left.
    pub fn without(self, pane: Pane) -> Option<Node> {
        match self {
            Node::Leaf(leaf) if leaf == pane => None,
            Node::Leaf(_) => Some(self),
            Node::Split { axis, ratio, first, second } => match (first.without(pane), second.without(pane)) {
                (Some(first), Some(second)) => {
                    Some(Node::Split { axis, ratio, first: Box::new(first), second: Box::new(second) })
                }
                (first, second) => first.or(second),
            },
        }
    }

    /// `self` with `pane` sharing the place of `beside`, after it along `axis`.
    pub fn split(self, beside: Pane, pane: Pane, axis: Axis) -> Node {
        match self {
            Node::Leaf(leaf) if leaf == beside => Node::Split {
                axis,
                ratio: 0.5,
                first: Box::new(Node::Leaf(leaf)),
                second: Box::new(Node::Leaf(pane)),
            },
            Node::Leaf(_) => self,
            Node::Split { axis: own, ratio, first, second } => Node::Split {
                axis: own,
                ratio,
                first: Box::new(first.split(beside, pane, axis)),
                second: Box::new(second.split(beside, pane, axis)),
            },
        }
    }

    /// `self` with the split at `path` giving its first half `ratio` of the space.
    pub fn resized(&mut self, path: &[Step], ratio: f32) {
        let Node::Split { ratio: own, first, second, .. } = self else {
            panic!("no split at the resized path");
        };
        match path.split_first() {
            None => *own = ratio.clamp(MIN_RATIO, 1. - MIN_RATIO),
            Some((Step::First, rest)) => first.resized(rest, ratio),
            Some((Step::Second, rest)) => second.resized(rest, ratio),
        }
    }
}

/// Adds `pane` to `layout`, beside everything there on the right.
pub fn added(layout: Option<Node>, pane: Pane) -> Node {
    match layout {
        None => Node::Leaf(pane),
        Some(node) => Node::Split {
            axis: Axis::Row,
            ratio: 0.6,
            first: Box::new(node),
            second: Box::new(Node::Leaf(pane)),
        },
    }
}
