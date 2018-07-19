use std::path::{Path, PathBuf};
use std::ffi::OsString;

pub fn get_obj_path<TP: AsRef<Path>, BP: AsRef<Path>, FP: AsRef<Path>>(target: TP, base: BP, file: FP) -> PathBuf {
    let mut base = base.as_ref();
    let mut parents = 0;
    loop {
        if let Ok(stripped) = file.as_ref().strip_prefix(base) {
            let mut second_part: OsString = format!("{}_", parents).into();
            second_part.push(stripped);
            break target.as_ref().join(second_part);
        }

        if let Some(parent) = base.parent() {
            base = parent;
            parents += 1;
        } else {
            let mut second_part: OsString = "x_".to_owned().into();
            second_part.push(file.as_ref());
            break target.as_ref().join(second_part);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::get_obj_path;

    #[test]
    fn basic() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/base/dir/file"), Path::new("/target/0_file"));
    }

    #[test]
    fn child() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/base/dir/child/file"), Path::new("/target/0_child/file"));
    }

    #[test]
    fn parent() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/base/file"), Path::new("/target/1_file"));
    }

    #[test]
    fn cousin() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/base/child/file"), Path::new("/target/1_child/file"));
    }

    #[test]
    fn grand_parent() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/file"), Path::new("/target/2_file"));
    }

    #[test]
    fn grand_cousin() {
        assert_eq!(get_obj_path("/target", "/base/dir", "/child1/child2/file"), Path::new("/target/2_child1/child2/file"));
    }
}
