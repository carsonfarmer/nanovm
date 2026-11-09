//! Trusted-fixture replay, not a network parser or production restore API.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn tree(history: &Path, sequence: u64, app: &str, object: &str) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut previous = BTreeMap::<PathBuf, Option<Vec<u8>>>::new();
    let mut parent = String::new();
    for number in 1..=sequence {
        let path = history.join(format!("{number:020}.tar"));
        let mut archive = tar::Archive::new(File::open(&path).unwrap());
        let mut next = BTreeMap::new();
        let mut inherited = Vec::new();
        for (index, entry) in archive.entries().unwrap().enumerate() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if index == 0 {
                assert_eq!(path, Path::new("nanovm.json"));
                let version: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(version["format"], 3);
                assert_eq!(version["app"], app);
                assert_eq!(version["object"], object);
                assert_eq!(version["sequence"], number);
                assert_eq!(version["parent"], parent);
            } else if let Ok(path) = path.strip_prefix("data") {
                if !path.as_os_str().is_empty() {
                    assert!(
                        next.insert(
                            path.to_owned(),
                            (!entry.header().entry_type().is_dir()).then_some(bytes)
                        )
                        .is_none()
                    );
                }
            } else if let Ok(path) = path.strip_prefix("inherit") {
                let metadata: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let mut data = previous
                    .remove(path)
                    .expect("inherited path must exist")
                    .expect("inherited file");
                data.resize(usize::try_from(metadata["length"].as_u64().unwrap()).unwrap(), 0);
                assert!(next.insert(path.to_owned(), Some(data)).is_none());
                inherited.push(path.to_owned());
            } else {
                let range = path.strip_prefix("ranges").unwrap();
                let mut fields = range.iter();
                let index: usize = fields.next().unwrap().to_str().unwrap().parse().unwrap();
                let offset: usize = fields.next().unwrap().to_str().unwrap().parse().unwrap();
                assert!(fields.next().is_none());
                let data = next.get_mut(&inherited[index]).unwrap().as_mut().unwrap();
                data[offset..offset + bytes.len()].copy_from_slice(&bytes);
            }
        }
        parent = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        previous = next;
    }
    previous
}
