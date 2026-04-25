use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub expanded: bool,
    pub depth: usize,
    pub children: Vec<TreeNode>,
    pub children_loaded: bool,
}

impl TreeNode {
    pub fn new(path: PathBuf, depth: usize) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let is_dir = path.is_dir();
        Self {
            path,
            name,
            is_dir,
            expanded: false,
            depth,
            children: Vec::new(),
            children_loaded: false,
        }
    }

    pub fn load_children(&mut self) {
        if self.children_loaded || !self.is_dir {
            return;
        }
        self.children_loaded = true;
        self.children.clear();

        let Ok(entries) = std::fs::read_dir(&self.path) else {
            return;
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files and common noise
            if name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "__pycache__"
            {
                continue;
            }

            let node = TreeNode::new(path, self.depth + 1);
            if node.is_dir {
                dirs.push(node);
            } else {
                files.push(node);
            }
        }

        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        self.children.extend(dirs);
        self.children.extend(files);
    }

    pub fn toggle_expand(&mut self) {
        if !self.is_dir {
            return;
        }
        if !self.expanded {
            self.load_children();
            self.expanded = true;
        } else {
            self.expanded = false;
        }
    }
}

pub struct FileTree {
    pub root: TreeNode,
    pub selected: usize,
    /// Flattened visible nodes for rendering/navigation
    visible_cache: Vec<(PathBuf, usize)>, // (path, depth)
}

impl FileTree {
    pub fn new(root_path: PathBuf) -> Self {
        let mut root = TreeNode::new(root_path, 0);
        root.expanded = true;
        root.load_children();

        let mut tree = Self {
            root,
            selected: 0,
            visible_cache: Vec::new(),
        };
        tree.rebuild_visible();
        tree
    }

    pub fn set_root(&mut self, path: PathBuf) {
        self.root = TreeNode::new(path, 0);
        self.root.expanded = true;
        self.root.load_children();
        self.selected = 0;
        self.rebuild_visible();
    }

    fn rebuild_visible(&mut self) {
        self.visible_cache.clear();
        Self::flatten_node(&self.root, &mut self.visible_cache);
    }

    fn flatten_node(node: &TreeNode, out: &mut Vec<(PathBuf, usize)>) {
        out.push((node.path.clone(), node.depth));
        if node.expanded {
            for child in &node.children {
                Self::flatten_node(child, out);
            }
        }
    }

    pub fn visible_nodes(&self) -> &[(PathBuf, usize)] {
        &self.visible_cache
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.visible_cache
            .get(self.selected)
            .map(|(p, _)| p.as_path())
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.visible_cache.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        Self::find_node(&self.root, path)
            .map(|n| n.expanded)
            .unwrap_or(false)
    }

    fn find_node<'a>(node: &'a TreeNode, target: &Path) -> Option<&'a TreeNode> {
        if node.path == target {
            return Some(node);
        }
        if node.expanded {
            for child in &node.children {
                if let Some(found) = Self::find_node(child, target) {
                    return Some(found);
                }
            }
        }
        None
    }

    pub fn toggle_selected(&mut self) {
        let Some((path, _)) = self.visible_cache.get(self.selected).cloned() else {
            return;
        };
        Self::toggle_at_path(&mut self.root, &path);
        self.rebuild_visible();
        // Clamp selection
        if self.selected >= self.visible_cache.len() {
            self.selected = self.visible_cache.len().saturating_sub(1);
        }
    }

    fn toggle_at_path(node: &mut TreeNode, target: &Path) -> bool {
        if node.path == target {
            node.toggle_expand();
            return true;
        }
        if node.expanded {
            for child in &mut node.children {
                if Self::toggle_at_path(child, target) {
                    return true;
                }
            }
        }
        false
    }

    #[allow(dead_code)]
    pub fn has_session_at(&self, dir: &Path, session_dirs: &[PathBuf]) -> bool {
        session_dirs.iter().any(|d| d == dir)
    }

    pub fn refresh(&mut self) {
        Self::refresh_node(&mut self.root);
        self.rebuild_visible();
        if self.selected >= self.visible_cache.len() {
            self.selected = self.visible_cache.len().saturating_sub(1);
        }
    }

    fn refresh_node(node: &mut TreeNode) {
        if node.expanded && node.is_dir {
            node.children_loaded = false;
            node.load_children();
            // Re-expand children that were expanded before
            // (simplified: just reload, losing expand state of children)
        }
    }
}
