use std::collections::HashMap;
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};

struct HeaderExtractor<R: BufRead> {
    reader: std::iter::Filter<std::iter::Map<io::Split<R>, fn(io::Result<Vec<u8>>) -> io::Result<Vec<u8>>>, fn(&io::Result<Vec<u8>>) -> bool>,
}

fn drop_lf(item: io::Result<Vec<u8>>) -> io::Result<Vec<u8>> {
    item.map(|mut item| { if item.last() == Some(&b'\n') { item.pop(); } item })
}

fn filter_headers(item: &io::Result<Vec<u8>>) -> bool {
    match *item {
        Ok(ref item) => {
            //eprintln!("Processing {:?}", item);
            if item.len() > 4 {
                &item[(item.len() - 2)..] == b".h" || &item[(item.len() - 4)..] == b".hpp"
            } else if item.len() > 2 {
                &item[(item.len() - 2)..] == b".h"
            } else {
                false
            }
        },
        Err(_) => true,
    }
}

impl<R: BufRead> HeaderExtractor<R> {
    pub fn new(reader: R) -> Self {
        HeaderExtractor {
            reader: reader
                .split(b' ')
                .map(drop_lf as fn(io::Result<Vec<u8>>) -> io::Result<Vec<u8>>)
                .filter(filter_headers as fn(&io::Result<Vec<u8>>) -> bool)
        }
    }
}

impl<R: BufRead> Iterator for HeaderExtractor<R> {
    type Item = io::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::os::unix::ffi::OsStringExt;

        self.reader.next().map(|item| item.map(std::ffi::OsString::from_vec).map(Into::into))
    }
}

fn header_to_unit<P: AsRef<Path>>(path: P) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    use std::ffi::OsString;
    use std::path::Path;

    let path = OsString::from(path.as_ref());
    let mut path = path.into_vec();
    if path.last() == Some(&b'h') {
        path.pop();
    } else {
        // hpp
        path.pop();
        path.pop();
        path.pop();
    }

    path.push(b'c');

    let path = OsString::from_vec(path);
    let path = if Path::new(&path).exists() {
        path
    } else {
        let mut path = path.into_vec();
        path.push(b'p');
        path.push(b'p');
        OsString::from_vec(path)
    };

    path.into()
}

/// Convert path to a .c(pp) file to a path to .o file.
pub fn unit_to_obj<P: AsRef<Path>>(path: P) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    use std::ffi::OsString;

    let path = OsString::from(path.as_ref());
    let mut path = path.into_vec();
    if path.last() == Some(&b'c') {
        path.pop();
    } else {
        // cpp
        path.pop();
        path.pop();
        path.pop();
    }

    path.push(b'o');
    let path = OsString::from_vec(path);
    path.into()
}

fn get_headers<P: AsRef<Path>>(file: P) -> io::Result<Vec<PathBuf>> {
    let mut cpp = std::process::Command::new("c++")
        .arg("-MM")
        .arg(file.as_ref())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let headers = HeaderExtractor::new(io::BufReader::new(cpp.stdout.take().expect("Stdout not set")));
    headers.collect()
}

/// Scans files in the project
pub fn scan_c_files<P: AsRef<Path>>(root_file: P) -> io::Result<HashMap<PathBuf, Vec<PathBuf>>> {
    let mut scanned_files = HashMap::new();

    let headers = get_headers(&root_file)?;
    scanned_files.insert(PathBuf::from(root_file.as_ref()), headers);

    let mut prev_file_count = 0;

    while prev_file_count != scanned_files.len() {
        prev_file_count = scanned_files.len();
        let candidates = scanned_files.iter().flat_map(|(_, headers)| headers.iter()).map(header_to_unit).filter(|file| !scanned_files.contains_key(file)).collect::<Vec<_>>();
        for (file, headers) in candidates.into_iter().map(|file| { let headers = get_headers(&file); (file, headers) }) {
            let headers = headers?;
            scanned_files.insert(file, headers);
        }
    }

    Ok(scanned_files)
}

fn is_older<P: AsRef<Path>, I: Iterator<Item=P>>(time: std::time::SystemTime, files: I) -> io::Result<bool> {
    for file in files {
        if std::fs::metadata(&file)?.modified()? > time {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Get the list of files that need recompilation
pub fn need_recompile<'a, P: AsRef<Path>>(target: P, sources: &'a HashMap<PathBuf, Vec<PathBuf>>) -> io::Result<Vec<&'a Path>> {
    let target_time = match std::fs::metadata(&target) {
        Ok(metadata) => Some(metadata.modified()?),
        Err(err) => if err.kind() == io::ErrorKind::NotFound {
            None
        } else {
            return Err(err);
        },
    };

    let mut result = Vec::new();

    for (source, headers) in sources {
        if let Some(target_time) = target_time {
            if is_older(target_time, Some(source).into_iter().chain(headers))? {
                result.push(source as &Path)
            }
        } else {
            result.push(source as &Path)
        }
    }

    Ok(result)
}
