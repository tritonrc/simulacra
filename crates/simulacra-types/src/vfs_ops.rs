//! Value types for the atomic `VirtualFs` operations: conditional writes,
//! rename, mutation batches and bounded listings.

/// What a mutation requires of its target before it may run. `MustMatch`
/// applies to files; on a directory it does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    Unconditional,
    MustBeAbsent,
    /// The target's current revision must equal this value.
    MustMatch(u64),
}

/// One item of an atomic batch. A batch commits every item or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsMutation {
    Write {
        path: String,
        data: Vec<u8>,
        precondition: Precondition,
    },
    Mkdir {
        path: String,
    },
    /// Removes a file, or a directory and everything under it.
    Remove {
        path: String,
        precondition: Precondition,
    },
    /// `to` must not exist; the precondition applies to `from`.
    Rename {
        from: String,
        to: String,
        precondition: Precondition,
    },
}

impl FsMutation {
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Self::Write { path, .. } | Self::Mkdir { path } | Self::Remove { path, .. } => {
                vec![path.as_str()]
            }
            Self::Rename { from, to, .. } => vec![from.as_str(), to.as_str()],
        }
    }
}

/// A directory listing that a backend may cut short.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<String>,
    pub truncated: bool,
}

impl DirListing {
    /// A listing nothing cut short: for a layer that computes the listing
    /// itself, so no backend bound applies.
    pub fn complete(entries: Vec<String>) -> Self {
        Self {
            entries,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};

    #[test]
    fn mutation_paths_lists_every_path_it_touches() {
        let rename = FsMutation::Rename {
            from: "/a".into(),
            to: "/b".into(),
            precondition: Precondition::Unconditional,
        };
        assert_eq!(rename.paths(), vec!["/a", "/b"]);
        let write = FsMutation::Write {
            path: "/c".into(),
            data: vec![1],
            precondition: Precondition::MustBeAbsent,
        };
        assert_eq!(write.paths(), vec!["/c"]);
        let mkdir = FsMutation::Mkdir { path: "/d".into() };
        assert_eq!(mkdir.paths(), vec!["/d"]);
        let remove = FsMutation::Remove {
            path: "/e".into(),
            precondition: Precondition::MustMatch(2),
        };
        assert_eq!(remove.paths(), vec!["/e"]);
    }

    #[test]
    fn metadata_constructors_leave_revision_and_media_type_unset() {
        let file = FsMetadata::file(7);
        assert!(file.is_file && !file.is_dir);
        assert_eq!(
            (file.size, file.revision, file.media_type.as_deref()),
            (7, None, None)
        );
        let dir = FsMetadata::dir();
        assert!(dir.is_dir && !dir.is_file && dir.size == 0);
        assert_eq!((dir.revision, dir.media_type.as_deref()), (None, None));
        assert_eq!(FsMetadata::file(1).with_revision(9).revision, Some(9));
    }

    #[test]
    fn with_revision_keeps_the_rest_of_the_metadata() {
        let meta = FsMetadata::file(12).with_revision(3);
        assert!(meta.is_file && !meta.is_dir);
        assert_eq!((meta.size, meta.revision), (12, Some(3)));
    }

    #[test]
    fn metadata_without_new_fields_still_deserializes() {
        let old = r#"{"is_file":true,"is_dir":false,"size":3}"#;
        let meta: FsMetadata = serde_json::from_str(old).unwrap();
        assert_eq!((meta.size, meta.revision), (3, None));
        assert_eq!(meta.media_type, None);
    }

    #[test]
    fn metadata_round_trips_revision_and_media_type() {
        let meta = FsMetadata {
            is_file: true,
            is_dir: false,
            size: 5,
            revision: Some(11),
            media_type: Some("text/markdown".into()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: FsMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.revision, Some(11));
        assert_eq!(back.media_type.as_deref(), Some("text/markdown"));
        assert_eq!(back.size, 5);
    }

    #[test]
    fn new_error_kinds_name_themselves_and_carry_their_detail() {
        let cases = [
            (VfsError::Unsupported("x1".into()), "unsupported: x1"),
            (VfsError::Conflict("x2".into()), "conflict: x2"),
            (VfsError::QuotaExceeded("x3".into()), "quota exceeded: x3"),
            (
                VfsError::DeadlineExceeded("x4".into()),
                "deadline exceeded: x4",
            ),
            (VfsError::OutcomeUnknown("x5".into()), "outcome unknown: x5"),
            (
                VfsError::FilesystemRestored("x6".into()),
                "filesystem restored: x6",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    /// Implements only the required methods, so every new method is a default.
    /// Every basic call is logged, so a default that emulates through them
    /// leaves a trace.
    #[derive(Default)]
    struct Bare {
        basic_calls: Mutex<Vec<String>>,
    }

    impl Bare {
        fn log(&self, call: String) {
            self.basic_calls.lock().unwrap().push(call);
        }
    }

    impl VirtualFs for Bare {
        fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
            self.log(format!("read {p}"));
            Err(VfsError::NotFound(p.into()))
        }
        fn write(&self, p: &str, d: &[u8]) -> Result<(), VfsError> {
            self.log(format!("write {p} {}", d.len()));
            Ok(())
        }
        fn exists(&self, p: &str) -> bool {
            self.log(format!("exists {p}"));
            false
        }
        fn list_dir(&self, p: &str) -> Result<Vec<String>, VfsError> {
            self.log(format!("list_dir {p}"));
            if p == "/missing" {
                return Err(VfsError::NotFound(p.into()));
            }
            Ok(vec!["x".into()])
        }
        fn mkdir(&self, p: &str) -> Result<(), VfsError> {
            self.log(format!("mkdir {p}"));
            Ok(())
        }
        fn remove(&self, p: &str) -> Result<(), VfsError> {
            self.log(format!("remove {p}"));
            Ok(())
        }
        fn metadata(&self, p: &str) -> Result<FsMetadata, VfsError> {
            self.log(format!("metadata {p}"));
            Err(VfsError::NotFound(p.into()))
        }
        fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
            self.log("snapshot".into());
            Ok(VfsSnapshot { data: vec![] })
        }
        fn restore(&self, _: &VfsSnapshot) -> Result<(), VfsError> {
            self.log("restore".into());
            Ok(())
        }
    }

    #[test]
    fn defaults_refuse_atomic_operations_instead_of_emulating_them() {
        let fs = Bare::default();
        assert!(matches!(
            fs.rename("/a", "/b"),
            Err(VfsError::Unsupported(_))
        ));
        assert!(matches!(
            fs.write_if("/a", b"x", Precondition::MustBeAbsent),
            Err(VfsError::Unsupported(_))
        ));
        assert!(matches!(fs.apply_batch(&[]), Err(VfsError::Unsupported(_))));
        let listing = fs.list_dir_bounded("/").unwrap();
        assert_eq!(
            listing,
            DirListing {
                entries: vec!["x".into()],
                truncated: false
            }
        );
    }

    #[test]
    fn refusing_defaults_leave_the_filesystem_untouched() {
        // A refusal that quietly wrote, removed or created anything would be
        // an emulation; the log catches any basic call a default makes.
        let fs = Bare::default();
        let batch = [
            FsMutation::Write {
                path: "/w".into(),
                data: vec![1],
                precondition: Precondition::Unconditional,
            },
            FsMutation::Remove {
                path: "/r".into(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Mkdir { path: "/d".into() },
        ];
        assert!(matches!(
            fs.write_if("/a", b"x", Precondition::Unconditional),
            Err(VfsError::Unsupported(_))
        ));
        assert!(matches!(
            fs.rename("/a", "/b"),
            Err(VfsError::Unsupported(_))
        ));
        assert!(matches!(
            fs.apply_batch(&batch),
            Err(VfsError::Unsupported(_))
        ));
        assert_eq!(*fs.basic_calls.lock().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn bounded_listing_default_passes_list_dir_errors_through() {
        let err = Bare::default().list_dir_bounded("/missing").unwrap_err();
        assert!(matches!(err, VfsError::NotFound(p) if p == "/missing"));
    }

    /// Overrides every new method with a distinctive answer, so a wrapper
    /// that dropped its forward would show the trait default instead.
    #[derive(Default)]
    struct Distinct {
        calls: Mutex<Vec<String>>,
    }

    impl VirtualFs for Distinct {
        fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
            Err(VfsError::NotFound(p.into()))
        }
        fn write(&self, _: &str, _: &[u8]) -> Result<(), VfsError> {
            Ok(())
        }
        fn exists(&self, _: &str) -> bool {
            false
        }
        fn list_dir(&self, _: &str) -> Result<Vec<String>, VfsError> {
            Ok(vec!["from-list-dir".into()])
        }
        fn mkdir(&self, _: &str) -> Result<(), VfsError> {
            Ok(())
        }
        fn remove(&self, _: &str) -> Result<(), VfsError> {
            Ok(())
        }
        fn metadata(&self, p: &str) -> Result<FsMetadata, VfsError> {
            Err(VfsError::NotFound(p.into()))
        }
        fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
            Ok(VfsSnapshot { data: vec![] })
        }
        fn restore(&self, _: &VfsSnapshot) -> Result<(), VfsError> {
            Ok(())
        }
        fn write_if(&self, p: &str, d: &[u8], pre: Precondition) -> Result<(), VfsError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("write_if {p} {} {pre:?}", d.len()));
            Err(VfsError::Conflict("from-write-if".into()))
        }
        fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rename {from} {to}"));
            Err(VfsError::QuotaExceeded("from-rename".into()))
        }
        fn apply_batch(&self, m: &[FsMutation]) -> Result<(), VfsError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("batch {}", m.len()));
            Err(VfsError::OutcomeUnknown("from-batch".into()))
        }
        fn list_dir_bounded(&self, _: &str) -> Result<DirListing, VfsError> {
            Ok(DirListing {
                entries: vec!["from-bounded".into()],
                truncated: true,
            })
        }
    }

    #[test]
    fn arc_dyn_forwards_every_new_method_to_the_inner_filesystem() {
        let inner = Arc::new(Distinct::default());
        let fs: Arc<dyn VirtualFs> = inner.clone();

        assert!(matches!(
            fs.rename("/a", "/b"),
            Err(VfsError::QuotaExceeded(m)) if m == "from-rename"
        ));
        assert!(matches!(
            fs.write_if("/c", b"xy", Precondition::MustMatch(6)),
            Err(VfsError::Conflict(m)) if m == "from-write-if"
        ));
        let batch = [
            FsMutation::Mkdir { path: "/d".into() },
            FsMutation::Mkdir { path: "/e".into() },
        ];
        assert!(matches!(
            fs.apply_batch(&batch),
            Err(VfsError::OutcomeUnknown(m)) if m == "from-batch"
        ));
        let listing = fs.list_dir_bounded("/").unwrap();
        assert_eq!(listing.entries, vec!["from-bounded".to_string()]);
        assert!(listing.truncated);

        assert_eq!(
            *inner.calls.lock().unwrap(),
            vec![
                "rename /a /b".to_string(),
                "write_if /c 2 MustMatch(6)".to_string(),
                "batch 2".to_string(),
            ]
        );
    }

    #[test]
    fn arc_dyn_of_a_bare_filesystem_still_refuses() {
        let fs: Arc<dyn VirtualFs> = Arc::new(Bare::default());
        assert!(matches!(
            fs.rename("/a", "/b"),
            Err(VfsError::Unsupported(_))
        ));
        assert_eq!(
            fs.list_dir_bounded("/").unwrap().entries,
            vec!["x".to_string()]
        );
    }
}
