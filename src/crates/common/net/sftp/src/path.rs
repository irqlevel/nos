//! A client's path made one the files take.

use alloc::string::String;

/// `path` made absolute against `/` -- the home every client has, there
/// being one user -- with every `.` and empty name taken out, and every
/// `..` taking out the name before it, never climbing above `/`. Nothing
/// is looked up: a path that names nothing comes out as well. None when
/// there is no memory for it.
pub fn normalize(path: &str) -> Option<String> {
    /* Every name kept is the input's, with one slash before it: never
       longer than the input and a slash, so nothing below grows it */
    let mut out = String::new();
    out.try_reserve_exact(path.len() + 1).ok()?;
    for name in path.split('/') {
        match name {
            "" | "." => {}
            ".." => {
                let cut = out.rfind('/').unwrap_or(0);
                out.truncate(cut);
            }
            name => {
                out.push('/');
                out.push_str(name);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

/// As `normalize`, a leading `~` -- a client's word for its home -- taken
/// as `/`: what expand-path@openssh.com asks of a path.
pub fn expand(path: &str) -> Option<String> {
    let rest = if path == "~" { "" } else { path.strip_prefix("~/").unwrap_or(path) };
    normalize(rest)
}
