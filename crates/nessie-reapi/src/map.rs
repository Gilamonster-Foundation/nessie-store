//! Proto ↔ native translation for `ActionResult` and `Directory`.
//!
//! The honest field-coverage boundary: the native [`ActionResult`] carries
//! `outputs` / `exit_code` / `stdout_digest` / `stderr_digest` **only**;
//! `output_directories`, symlinks, and `execution_metadata` are rejected on write
//! until the core type grows them (a tracked follow-on).

use crate::boundary::Sha256Boundary;
use crate::reapi;
use crate::size::SizeSource;
use nessie_backend_core::{ActionResult, Digest, OutputFile};
use prost::Message;
use std::collections::BTreeMap;
use tonic::Status;

/// Native → REAPI: the emit path (`GetActionResult`). Fills each output's REAPI
/// `size_bytes` from `sizes`.
///
/// # Errors
///
/// Propagates a [`SizeSource`] error (e.g. a referenced output blob is absent).
pub fn ar_to_reapi(
    ar: &ActionResult,
    boundary: &Sha256Boundary,
    sizes: &dyn SizeSource,
) -> Result<reapi::ActionResult, Status> {
    let mut output_files = Vec::with_capacity(ar.outputs.len());
    for (path, file) in &ar.outputs {
        let size = sizes.size_of(&file.digest)?;
        output_files.push(reapi::OutputFile {
            path: path.clone(),
            digest: Some(boundary.to_reapi(&file.digest, size)),
            is_executable: file.is_executable,
            contents: Vec::new(),
            node_properties: None,
        });
    }
    Ok(reapi::ActionResult {
        output_files,
        exit_code: ar.exit_code,
        stdout_digest: opt_to_reapi(ar.stdout_digest.as_ref(), boundary, sizes)?,
        stderr_digest: opt_to_reapi(ar.stderr_digest.as_ref(), boundary, sizes)?,
        ..Default::default()
    })
}

fn opt_to_reapi(
    digest: Option<&Digest>,
    boundary: &Sha256Boundary,
    sizes: &dyn SizeSource,
) -> Result<Option<reapi::Digest>, Status> {
    match digest {
        None => Ok(None),
        Some(d) => Ok(Some(boundary.to_reapi(d, sizes.size_of(d)?))),
    }
}

/// REAPI → native: the write path (`UpdateActionResult`). Rejects fields the native
/// `ActionResult` cannot represent.
///
/// # Errors
///
/// [`tonic::Status::invalid_argument`] if the result carries output directories,
/// symlinks, execution metadata, inline stdout/stderr, a missing output digest, a
/// bad digest, or a duplicate output path.
pub fn ar_from_reapi(
    ar: &reapi::ActionResult,
    boundary: &Sha256Boundary,
) -> Result<ActionResult, Status> {
    if !ar.output_directories.is_empty() || !ar.output_symlinks.is_empty() {
        return Err(Status::invalid_argument(
            "output_directories / output_symlinks are not yet representable",
        ));
    }
    if ar.execution_metadata.is_some() {
        return Err(Status::invalid_argument(
            "execution_metadata is not represented",
        ));
    }
    if !ar.stdout_raw.is_empty() || !ar.stderr_raw.is_empty() {
        return Err(Status::invalid_argument(
            "inline stdout_raw/stderr_raw is not supported; upload the bytes and use a digest",
        ));
    }

    let mut outputs = BTreeMap::new();
    for file in &ar.output_files {
        let digest = file
            .digest
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("output_file is missing its digest"))?;
        let native = boundary.to_native(digest)?;
        if outputs
            .insert(
                file.path.clone(),
                OutputFile {
                    digest: native,
                    is_executable: file.is_executable,
                },
            )
            .is_some()
        {
            return Err(Status::invalid_argument(format!(
                "duplicate output path {:?}",
                file.path
            )));
        }
    }

    Ok(ActionResult {
        outputs,
        exit_code: ar.exit_code,
        stdout_digest: ar
            .stdout_digest
            .as_ref()
            .map(|d| boundary.to_native(d))
            .transpose()?,
        stderr_digest: ar
            .stderr_digest
            .as_ref()
            .map(|d| boundary.to_native(d))
            .transpose()?,
    })
}

/// The child digests of a stored REAPI `Directory` blob — its file and sub-directory
/// digests. Used by `GetTree` recursion and (later) a durable-GC Directory resolver.
///
/// # Errors
///
/// [`tonic::Status::invalid_argument`] if `bytes` is not a decodable `Directory`, or
/// carries a malformed child digest.
pub fn dir_child_digests(bytes: &[u8], boundary: &Sha256Boundary) -> Result<Vec<Digest>, Status> {
    let dir = reapi::Directory::decode(bytes)
        .map_err(|e| Status::invalid_argument(format!("not a REAPI Directory: {e}")))?;
    let mut out = Vec::with_capacity(dir.files.len() + dir.directories.len());
    for f in &dir.files {
        if let Some(d) = &f.digest {
            out.push(boundary.to_native(d)?);
        }
    }
    for d in &dir.directories {
        if let Some(dg) = &d.digest {
            out.push(boundary.to_native(dg)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::DigestAlgo;
    use std::collections::HashMap;

    /// A hand-built size table — the `SizeSource` seam in action, no CAS needed.
    struct TableSizes(HashMap<Digest, u64>);
    impl SizeSource for TableSizes {
        fn size_of(&self, digest: &Digest) -> Result<u64, Status> {
            self.0
                .get(digest)
                .copied()
                .ok_or_else(|| Status::not_found("no size"))
        }
    }

    fn sha(tag: &[u8]) -> Digest {
        Digest::compute_with(DigestAlgo::Sha256, tag)
    }

    #[test]
    fn action_result_round_trips_through_reapi() {
        let out = sha(b"out");
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "bin/app".to_string(),
            OutputFile {
                digest: out.clone(),
                is_executable: true,
            },
        );
        let native = ActionResult {
            outputs,
            exit_code: 0,
            stdout_digest: Some(sha(b"stdout")),
            stderr_digest: None,
        };
        let sizes = TableSizes([(out, 3), (sha(b"stdout"), 6)].into_iter().collect());
        let boundary = Sha256Boundary;
        let proto = ar_to_reapi(&native, &boundary, &sizes).expect("to reapi");
        assert_eq!(proto.output_files.len(), 1);
        assert!(proto.output_files[0].is_executable);
        assert_eq!(proto.output_files[0].digest.as_ref().unwrap().size_bytes, 3);
        // Back to native — the digests and structure survive.
        let back = ar_from_reapi(&proto, &boundary).expect("from reapi");
        assert_eq!(back, native);
    }

    #[test]
    fn from_reapi_rejects_output_directories() {
        let proto = reapi::ActionResult {
            output_directories: vec![reapi::OutputDirectory::default()],
            ..Default::default()
        };
        assert!(ar_from_reapi(&proto, &Sha256Boundary).is_err());
    }

    #[test]
    fn dir_child_digests_reads_a_stored_directory() {
        let child_file = Sha256Boundary.to_reapi(&sha(b"file"), 4);
        let child_dir = Sha256Boundary.to_reapi(&sha(b"subdir"), 10);
        let dir = reapi::Directory {
            files: vec![reapi::FileNode {
                name: "f".to_string(),
                digest: Some(child_file),
                is_executable: false,
                node_properties: None,
            }],
            directories: vec![reapi::DirectoryNode {
                name: "d".to_string(),
                digest: Some(child_dir),
            }],
            ..Default::default()
        };
        let bytes = dir.encode_to_vec();
        let children = dir_child_digests(&bytes, &Sha256Boundary).expect("decode");
        assert!(children.contains(&sha(b"file")));
        assert!(children.contains(&sha(b"subdir")));
        assert_eq!(children.len(), 2);
    }
}
