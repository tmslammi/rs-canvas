//! Tools: what a pointer gesture means.

/// The active tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Select,
    Hand,
    Rectangle,
    Diamond,
    Ellipse,
    Freehand,
    Arrow,
    Line,
    Text,
    Sticky,
    StickyStack,
    Frame,
    Eraser,
}

impl Tool {
    pub const ALL: [Tool; 13] = [
        Tool::Select,
        Tool::Hand,
        Tool::Rectangle,
        Tool::Diamond,
        Tool::Ellipse,
        Tool::Freehand,
        Tool::Arrow,
        Tool::Line,
        Tool::Text,
        Tool::Sticky,
        Tool::StickyStack,
        Tool::Frame,
        Tool::Eraser,
    ];

    /// Single-key tool shortcut.
    pub fn shortcut(self) -> &'static str {
        match self {
            Tool::Select => "v",
            Tool::Hand => "h",
            Tool::Rectangle => "r",
            Tool::Diamond => "d",
            Tool::Ellipse => "o",
            Tool::Freehand => "p",
            Tool::Arrow => "a",
            Tool::Line => "l",
            Tool::Text => "t",
            Tool::Sticky => "n",
            Tool::StickyStack => "k",
            Tool::Frame => "f",
            Tool::Eraser => "e",
        }
    }

    pub fn from_shortcut(key: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.shortcut() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Hand => "Hand",
            Tool::Rectangle => "Rectangle",
            Tool::Diamond => "Diamond",
            Tool::Ellipse => "Ellipse",
            Tool::Freehand => "Draw",
            Tool::Arrow => "Arrow",
            Tool::Line => "Line",
            Tool::Text => "Text",
            Tool::Sticky => "Sticky note",
            Tool::StickyStack => "Sticky stack",
            Tool::Frame => "Frame",
            Tool::Eraser => "Eraser",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Hand => "hand",
            Tool::Rectangle => "rectangle",
            Tool::Diamond => "diamond",
            Tool::Ellipse => "ellipse",
            Tool::Freehand => "freehand",
            Tool::Arrow => "arrow",
            Tool::Line => "line",
            Tool::Text => "text",
            Tool::Sticky => "sticky",
            Tool::StickyStack => "stickyStack",
            Tool::Frame => "frame",
            Tool::Eraser => "eraser",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_round_trip() {
        for tool in Tool::ALL {
            assert_eq!(Tool::from_shortcut(tool.shortcut()), Some(tool));
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Tool::ALL.len());
    }
}
