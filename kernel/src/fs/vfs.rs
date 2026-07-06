use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_intf::KError;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::Spinlock;

pub const MAX_SYMLINK_DEPTH: usize = 8;

pub const MODE_FILE: u16    = 1 << 0;
pub const MODE_DIR: u16     = 1 << 1;
pub const MODE_SYMLINK: u16 = 1 << 2;

pub enum FileData {
    Static(&'static [u8]),
    Owned(Vec<u8>)
}

impl FileData {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            FileData::Static(s) => s,
            FileData::Owned(v) => v.as_slice()
        }
    }

    pub fn len(&self) -> usize {
        match self {
            FileData::Static(s) => s.len(),
            FileData::Owned(v) => v.len()
        }
    }

    // Promote to Owned so the caller can mutate the data.
    pub fn make_owned(&mut self) {
        if let FileData::Static(s) = self {
            *self = FileData::Owned(s.to_vec());
        }
    }
}

pub enum NodeKind {
    File { data: FileData, open_count: usize },
    Dir { children: BTreeMap<String, VfsNodeRef>, open_count: usize },
    Symlink { target: String }
}

#[derive(Debug, Clone, Copy)]
pub struct FileAttrs {
    pub mode: u16,
    pub size: u64
}

pub enum ProbeStep {
    Found { attrs: FileAttrs, symlink_target: Option<String> },
    // `dir`/`remaining` are both relative to the current backend/mount.
    Symlink { dir: String, target: String, remaining: String }
}

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum HandleStatType {
    File = 0,
    Device = 1,
    Thread = 2,
    Process = 3,
    Sync = 4,
    PipeRead = 5,
    PipeWrite = 6
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileStat {
    pub size: u64,
    pub mode: u16,
    pub handle_type: HandleStatType
}

pub struct VfsNode {
    pub attrs: FileAttrs,
    pub kind: NodeKind
}

pub type VfsNodeRef = Arc<Spinlock<VfsNode>, PoolAllocatorGlobal>;

fn new_node(kind: NodeKind, mode: u16, size: u64) -> VfsNodeRef {
    Arc::new_in(
        Spinlock::new(VfsNode { attrs: FileAttrs { mode, size }, kind }),
        PoolAllocatorGlobal
    )
}

fn new_dir(mode: u16) -> VfsNodeRef {
    new_node(NodeKind::Dir { children: BTreeMap::new(), open_count: 0 }, mode | MODE_DIR, 0)
}

fn new_file_node(data: FileData, mode: u16) -> VfsNodeRef {
    let size = data.len() as u64;
    new_node(NodeKind::File { data, open_count: 0 }, mode | MODE_FILE, size)
}

fn new_symlink_node(target: String) -> VfsNodeRef {
    let sz = target.len() as u64;
    new_node(NodeKind::Symlink { target }, MODE_SYMLINK, sz)
}

pub fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => { parts.pop(); }
            s => parts.push(s)
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::new();
        for p in &parts {
            out.push('/');
            out.push_str(p);
        }
        out
    }
}

pub fn make_absolute(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        normalize_path(path)
    } else {
        normalize_path(&alloc::format!("{}/{}", cwd, path))
    }
}

// Returns (parent_path, file_name) for an absolute normalised path.
pub(crate) fn split_parent(path: &str) -> (&str, &str) {
    if path == "/" { return ("/", ""); }
    match path.rfind('/') {
        Some(0) => (&path[..1], &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path)
    }
}

// Rebuild path string from a list of traversed component names.
fn rebuild_path(components: &[&str]) -> String {
    if components.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for c in components {
        out.push('/');
        out.push_str(c);
    }
    out
}

pub struct Vfs {
    root: VfsNodeRef
}

impl Vfs {
    pub fn new() -> Self {
        // For now, our mode doesn't really have any meaning
        Vfs { root: new_dir(0) }
    }

    // Populate from boot-time initfs flat maps. Creates intermediate dirs as needed.
    pub fn populate(
        &mut self,
        files: &BTreeMap<&'static str, &'static [u8]>,
        symlinks: &BTreeMap<&'static str, &'static str>
    ) {
        for (&path, &data) in files {
            let abs = normalize_path(path);
            let (parent_path, name) = split_parent(&abs);
            if name.is_empty() { continue; }
            self.ensure_dirs(parent_path);
            if let Ok(parent) = self.raw_get(parent_path) {
                let mut pg = parent.lock();
                if let NodeKind::Dir { ref mut children, .. } = pg.kind {
                    children.entry(name.to_owned())
                        .or_insert_with(|| new_file_node(FileData::Static(data), 0));
                }
            }
        }

        for (&link_path, &target) in symlinks {
            let abs = normalize_path(link_path);
            let (parent_path, name) = split_parent(&abs);
            if name.is_empty() { continue; }
            self.ensure_dirs(parent_path);
            if let Ok(parent) = self.raw_get(parent_path) {
                let mut pg = parent.lock();
                if let NodeKind::Dir { ref mut children, .. } = pg.kind {
                    children.entry(name.to_owned())
                        .or_insert_with(|| new_symlink_node(normalize_path(target)));
                }
            }
        }
    }

    fn ensure_dirs(&self, path: &str) {
        if path == "/" || path.is_empty() { return; }
        let (parent, name) = split_parent(path);
        self.ensure_dirs(parent);
        if name.is_empty() { return; }
        if let Ok(p) = self.raw_get(parent) {
            let mut pg = p.lock();
            if let NodeKind::Dir { ref mut children, .. } = pg.kind {
                children.entry(name.to_owned()).or_insert_with(|| new_dir(0));
            }
        }
    }

    // Walk without symlink resolution (for populate only).
    fn raw_get(&self, path: &str) -> Result<VfsNodeRef, KError> {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = Arc::clone(&self.root);
        for comp in comps {
            let next = {
                let g = cur.lock();
                match &g.kind {
                    NodeKind::Dir { children, .. } => children.get(comp).map(Arc::clone).ok_or(KError::NotFound)?,
                    _ => return Err(KError::NotADirectory)
                }
            };
            cur = next;
        }
        Ok(cur)
    }

    pub fn probe(&self, path: &str, follow_final: bool) -> Result<ProbeStep, KError> {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = Arc::clone(&self.root);

        if comps.is_empty() {
            let g = cur.lock();
            let mut attrs = g.attrs;
            match &g.kind {
                NodeKind::File { data, .. } => attrs.size = data.len() as u64,
                NodeKind::Dir { children, .. } => attrs.size = children.len() as u64,
                NodeKind::Symlink { target } => attrs.size = target.len() as u64
            }
            return Ok(ProbeStep::Found { attrs, symlink_target: None });
        }

        for (idx, &comp) in comps.iter().enumerate() {
            let is_last = idx == comps.len() - 1;

            let child = {
                let g = cur.lock();
                match &g.kind {
                    NodeKind::Dir { children, .. } => {
                        children.get(comp).map(Arc::clone).ok_or(KError::NotFound)?
                    }
                    // `cur` only ever becomes a symlink target below via an
                    // early return, so it can never itself be a symlink here.
                    _ => return Err(KError::NotADirectory)
                }
            };

            let is_symlink = {
                let g = child.lock();
                matches!(&g.kind, NodeKind::Symlink { .. })
            };

            if is_symlink && (!is_last || follow_final) {
                let target = {
                    let g = child.lock();
                    if let NodeKind::Symlink { target } = &g.kind { target.clone() } else { unreachable!() }
                };
                let dir = rebuild_path(&comps[..idx]);
                let remaining = if is_last { String::new() } else { comps[idx + 1..].join("/") };
                crate::fs_log!("memory backend: probe path={} hit symlink dir={} target={} remaining={}", path, dir, target, remaining);
                return Ok(ProbeStep::Symlink { dir, target, remaining });
            }

            if is_last {
                let g = child.lock();
                let mut attrs = g.attrs;
                let symlink_target = match &g.kind {
                    NodeKind::File { data, .. } => { attrs.size = data.len() as u64; None }
                    NodeKind::Dir { children, .. } => { attrs.size = children.len() as u64; None }
                    NodeKind::Symlink { target } => { attrs.size = target.len() as u64; Some(target.clone()) }
                };
                return Ok(ProbeStep::Found { attrs, symlink_target });
            }

            cur = child;
        }

        unreachable!()
    }

    // Compute the path to follow after encountering a symlink.
    // `dir`    : absolute path of the directory containing the symlink.
    // `target` : the symlink's target (absolute or relative).
    // `rest`   : remaining path components after the symlink (may be empty).
    pub fn join_symlink_target(dir: &str, target: &str, rest: &str) -> String {
        let base = if target.starts_with('/') {
            target.to_owned()
        } else {
            alloc::format!("{}/{}", dir, target)
        };
        let full = if rest.is_empty() {
            base
        } else {
            alloc::format!("{}/{}", base, rest)
        };
        normalize_path(&full)
    }

    // Open an already fully-resolved (no remaining symlinks) path for
    // file/dir access. Returns (node, ancestors, is_dir) with open_counts
    // incremented on all — this prevents the dir it is in from being
    // deleted/renamed while a file within it is open.
    pub fn open_at(&self, path: &str) -> Result<(VfsNodeRef, Vec<VfsNodeRef>, bool), KError> {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = Arc::clone(&self.root);
        let mut ancestors: Vec<VfsNodeRef> = Vec::new();

        for &comp in &comps {
            let next = {
                let g = cur.lock();
                match &g.kind {
                    NodeKind::Dir { children, .. } => children.get(comp).map(Arc::clone).ok_or(KError::NotFound)?,
                    _ => return Err(KError::NotADirectory)
                }
            };
            ancestors.push(Arc::clone(&cur));
            cur = next;
        }

        let is_dir = {
            let mut g = cur.lock();
            match &mut g.kind {
                NodeKind::File { open_count, .. } => {
                    *open_count += 1;
                    false
                }
                NodeKind::Dir { open_count, .. } => {
                    *open_count += 1;
                    true
                }
                NodeKind::Symlink { .. } => {
                    // Caller must have already resolved this.
                    return Err(KError::TooManySymlinks);
                }
            }
        };

        for anc in &ancestors {
            let mut g = anc.lock();
            if let NodeKind::Dir { open_count, .. } = &mut g.kind {
                *open_count += 1;
            }
        }

        Ok((cur, ancestors, is_dir))
    }

    // Decrement open_count on `node` and all `ancestors`
    pub fn close(node: &VfsNodeRef, ancestors: &[VfsNodeRef]) {
        {
            let mut g = node.lock();
            match &mut g.kind {
                NodeKind::File { open_count, .. } => {
                    *open_count = open_count.saturating_sub(1);
                }
                NodeKind::Dir { open_count, .. } => {
                    *open_count = open_count.saturating_sub(1);
                }
                NodeKind::Symlink { .. } => {}
            }
        }
        for anc in ancestors {
            let mut g = anc.lock();
            if let NodeKind::Dir { open_count, .. } = &mut g.kind {
                *open_count = open_count.saturating_sub(1);
            }
        }
    }

    // Whether anything is currently open anywhere within this vfs
    pub fn root_busy(&self) -> bool {
        let g = self.root.lock();
        match &g.kind {
            NodeKind::Dir { open_count, .. } => *open_count > 0,
            _ => false
        }
    }

    pub fn create_file_in(&self, parent_rel: &str, leaf: &str, mode: u16) -> Result<(), KError> {
        if leaf.is_empty() { return Err(KError::InvalidArgument); }
        let parent = self.raw_get(parent_rel)?;
        let mut pg = parent.lock();
        match &mut pg.kind {
            NodeKind::Dir { children, .. } => {
                if children.contains_key(leaf) { return Err(KError::FileExists); }
                children.insert(leaf.to_owned(), new_file_node(FileData::Owned(Vec::new()), mode));
                Ok(())
            }
            _ => Err(KError::NotADirectory)
        }
    }

    pub fn mkdir_in(&self, parent_rel: &str, leaf: &str, mode: u16) -> Result<(), KError> {
        if leaf.is_empty() { return Err(KError::InvalidArgument); }
        let parent = self.raw_get(parent_rel)?;
        let mut pg = parent.lock();
        match &mut pg.kind {
            NodeKind::Dir { children, .. } => {
                if children.contains_key(leaf) { return Err(KError::FileExists); }
                children.insert(leaf.to_owned(), new_dir(mode));
                Ok(())
            }
            _ => Err(KError::NotADirectory)
        }
    }

    pub fn create_symlink_in(&self, parent_rel: &str, leaf: &str, target: &str) -> Result<(), KError> {
        if leaf.is_empty() { return Err(KError::InvalidArgument); }
        let parent = self.raw_get(parent_rel)?;
        let mut pg = parent.lock();
        match &mut pg.kind {
            NodeKind::Dir { children, .. } => {
                if children.contains_key(leaf) { return Err(KError::FileExists); }
                children.insert(leaf.to_owned(), new_symlink_node(target.to_owned()));
                Ok(())
            }
            _ => Err(KError::NotADirectory)
        }
    }

    // Delete the leaf `leaf` of the already-resolved parent `parent_rel`.
    // Does NOT follow a symlink at `leaf` — deletes the symlink itself.
    pub fn delete_in(&self, parent_rel: &str, leaf: &str) -> Result<(), KError> {
        if leaf.is_empty() { return Err(KError::InvalidArgument); }

        let parent = self.raw_get(parent_rel)?;
        let mut pg = parent.lock();
        let children = match &mut pg.kind {
            NodeKind::Dir { children, .. } => children,
            _ => return Err(KError::NotADirectory)
        };
        let node = children.get(leaf).map(Arc::clone).ok_or(KError::NotFound)?;

        {
            let g = node.lock();
            match &g.kind {
                NodeKind::File { open_count, .. } if *open_count > 0 => return Err(KError::FileBusy),
                NodeKind::Dir { children: sub, open_count, .. } => {
                    if !sub.is_empty() { return Err(KError::NotEmpty); }
                    if *open_count > 0 { return Err(KError::FileBusy); }
                }
                _ => {}
            }
        }

        children.remove(leaf);
        Ok(())
    }

    // Rename `from_leaf` under the already-resolved `from_parent_rel` to
    // `to_leaf` under `to_parent_rel`. Does not follow a symlink at `from_leaf`.
    pub fn rename_in(
        &self,
        from_parent_rel: &str,
        from_name: &str,
        to_parent_rel: &str,
        to_name: &str
    ) -> Result<(), KError> {
        if from_name.is_empty() || to_name.is_empty() {
            return Err(KError::InvalidArgument);
        }

        let fp = self.raw_get(from_parent_rel)?;
        let tp = self.raw_get(to_parent_rel)?;

        if Arc::ptr_eq(&fp, &tp) {
            let mut guard = fp.lock();
            let children = match &mut guard.kind {
                NodeKind::Dir { children, .. } => children,
                _ => return Err(KError::NotADirectory)
            };
            let node = children.get(from_name).map(Arc::clone).ok_or(KError::NotFound)?;
            check_not_busy(&node)?;
            if to_name != from_name && children.contains_key(to_name) {
                return Err(KError::FileExists);
            }
            children.remove(from_name);
            children.insert(to_name.to_owned(), node);
        } else {
            // Always lock in a consistent order to avoid deadlock.
            // Use pointer comparison as a stable ordering key.
            let (first, second, src_is_first) =
                if Arc::as_ptr(&fp) < Arc::as_ptr(&tp) {
                    (&fp, &tp, true)
                } else {
                    (&tp, &fp, false)
                };

            let mut fg = first.lock();
            let mut sg = second.lock();

            let (from_children, to_children) = if src_is_first {
                (dir_children_mut(&mut fg.kind)?, dir_children_mut(&mut sg.kind)?)
            } else {
                let tc = dir_children_mut(&mut fg.kind)?;
                let fc = dir_children_mut(&mut sg.kind)?;
                (fc, tc)
            };

            let node = from_children.get(from_name).map(Arc::clone).ok_or(KError::NotFound)?;
            check_not_busy(&node)?;
            if to_children.contains_key(to_name) {
                return Err(KError::FileExists);
            }
            from_children.remove(from_name);
            to_children.insert(to_name.to_owned(), node);
        }

        Ok(())
    }

    pub fn readdir(node: &VfsNodeRef, ancestors: &[VfsNodeRef], offset: usize) -> Result<DirEntry, KError> {
        if offset == 0 {
            return Ok(dot_entry(".", node));
        }
        if offset == 1 {
            let parent = ancestors.last().unwrap_or(node);
            return Ok(dot_entry("..", parent));
        }
        let g = node.lock();
        match &g.kind {
            NodeKind::Dir { children, .. } => {
                let (name, child) = children.iter().nth(offset - 2)
                    .ok_or(KError::NoMoreEntries)?;
                let cg = child.lock();
                let (kind, symlink_target, size) = match &cg.kind {
                    NodeKind::File { data, .. } => (EntryType::File, None, data.len() as u64),
                    NodeKind::Dir { children, .. } => (EntryType::Dir, None, children.len() as u64),
                    NodeKind::Symlink { target } => (EntryType::Symlink, Some(target.clone()), target.len() as u64)
                };
                Ok(DirEntry {
                    name: name.clone(),
                    kind,
                    attrs: FileAttrs { mode: cg.attrs.mode, size },
                    symlink_target
                })
            }
            _ => Err(KError::NotADirectory)
        }
    }
}

fn dot_entry(name: &str, node: &VfsNodeRef) -> DirEntry {
    let g = node.lock();
    let size = match &g.kind {
        NodeKind::Dir { children, .. } => children.len() as u64,
        _ => 0
    };
    DirEntry {
        name: name.to_string(),
        kind: EntryType::Dir,
        attrs: FileAttrs { mode: g.attrs.mode, size },
        symlink_target: None
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum EntryType {
    File,
    Dir,
    Symlink
}

pub struct DirEntry {
    pub name: String,
    pub kind: EntryType,
    pub attrs: FileAttrs,
    pub symlink_target: Option<String>
}

fn check_not_busy(node: &VfsNodeRef) -> Result<(), KError> {
    let g = node.lock();
    match &g.kind {
        NodeKind::File { open_count, .. } if *open_count > 0 => Err(KError::FileBusy),
        NodeKind::Dir { open_count, .. } if *open_count > 0 => Err(KError::FileBusy),
        _ => Ok(())
    }
}

fn dir_children_mut(kind: &mut NodeKind) -> Result<&mut BTreeMap<String, VfsNodeRef>, KError> {
    match kind {
        NodeKind::Dir { children, .. } => Ok(children),
        _ => Err(KError::NotADirectory)
    }
}
