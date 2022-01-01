use std::time::Instant;
use std::{cmp::Ordering, sync::atomic::AtomicBool};

use git_features::progress::Progress;

use crate::multi_index::File;

///
pub mod integrity {
    use crate::multi_index::EntryIndex;

    /// Returned by [`multi_index::File::verify_integrity()`][crate::multi_index::File::verify_integrity()].
    #[derive(thiserror::Error, Debug)]
    #[allow(missing_docs)]
    pub enum Error {
        #[error("Object {id} should be at pack-offset {expected_pack_offset} but was found at {actual_pack_offset}")]
        PackOffsetMismatch {
            id: git_hash::ObjectId,
            expected_pack_offset: u64,
            actual_pack_offset: u64,
        },
        #[error(transparent)]
        MultiIndexChecksum(#[from] crate::multi_index::verify::checksum::Error),
        #[error(transparent)]
        IndexIntegrity(#[from] crate::index::verify::integrity::Error),
        #[error(transparent)]
        BundleInit(#[from] crate::bundle::init::Error),
        #[error("Counted {actual} objects, but expected {expected} as per multi-index")]
        UnexpectedObjectCount { actual: usize, expected: usize },
        #[error("{id} wasn't found in the index referenced in the multi-pack index")]
        OidNotFound { id: git_hash::ObjectId },
        #[error("The object id at multi-index entry {index} wasn't in order")]
        OutOfOrder { index: EntryIndex },
        #[error("The fan at index {index} is out of order as it's larger then the following value.")]
        Fan { index: usize },
        #[error("The multi-index claims to have no objects")]
        Empty,
        #[error("Interrupted")]
        Interrupted,
    }

    /// Returned by [`multi_index::File::verify_integrity()`][crate::multi_index::File::verify_integrity()].
    pub struct Outcome<P> {
        /// The computed checksum of the multi-index which matched the stored one.
        pub actual_index_checksum: git_hash::ObjectId,
        /// The for each entry in [`index_names()`][super::File::index_names()] provide the corresponding pack traversal outcome.
        pub pack_traverse_statistics: Vec<crate::index::traverse::Statistics>,
        /// The provided progress instance.
        pub progress: P,
    }

    /// Additional options to define how the integrity should be verified.
    pub struct Options<F> {
        /// The thoroughness of the verification
        pub verify_mode: crate::index::verify::Mode,
        /// The way to traverse packs
        pub traversal: crate::index::traverse::Algorithm,
        /// The amount of theads to use of `Some(N)`, with `None|Some(0)` using all available cores are used.
        pub thread_limit: Option<usize>,
        /// A function to create a pack cache
        pub make_pack_lookup_cache: F,
    }

    impl Default for Options<fn() -> crate::cache::Never> {
        fn default() -> Self {
            Options {
                verify_mode: Default::default(),
                traversal: Default::default(),
                thread_limit: None,
                make_pack_lookup_cache: || crate::cache::Never,
            }
        }
    }
}

///
pub mod checksum {
    /// Returned by [`multi_index::File::verify_checksum()`][crate::multi_index::File::verify_checksum()].
    pub type Error = crate::verify::checksum::Error;
}

impl File {
    /// Validate that our [`checksum()`][File::checksum()] matches the actual contents
    /// of this index file, and return it if it does.
    pub fn verify_checksum(
        &self,
        progress: impl Progress,
        should_interrupt: &AtomicBool,
    ) -> Result<git_hash::ObjectId, checksum::Error> {
        crate::verify::checksum_on_disk_or_mmap(
            self.path(),
            &self.data,
            self.checksum(),
            self.object_hash,
            progress,
            should_interrupt,
        )
    }

    /// Similar to [`verify_integrity()`][File::verify_integrity()] but without any deep inspection of objects.
    ///
    /// Instead we only validate the contents of the multi-index itself.
    pub fn verify_integrity_fast<P>(
        &self,
        progress: P,
        should_interrupt: &AtomicBool,
    ) -> Result<(git_hash::ObjectId, P), integrity::Error>
    where
        P: Progress,
    {
        self.verify_integrity_inner(progress, should_interrupt, false, integrity::Options::default())
            .map_err(|err| match err {
                crate::index::traverse::Error::Processor(err) => err,
                _ => unreachable!("BUG: no other error type is possible"),
            })
            .map(|o| (o.actual_index_checksum, o.progress))
    }

    /// Similar to [`crate::Bundle::verify_integrity()`] but checks all contained indices and their packs.
    ///
    /// Note that it's considered a failure if an index doesn't have a corresponding pack.
    pub fn verify_integrity<C, P, F>(
        &self,
        progress: P,
        should_interrupt: &AtomicBool,
        options: integrity::Options<F>,
    ) -> Result<integrity::Outcome<P>, crate::index::traverse::Error<integrity::Error>>
    where
        P: Progress,
        C: crate::cache::DecodeEntry,
        F: Fn() -> C + Send + Clone,
    {
        self.verify_integrity_inner(progress, should_interrupt, true, options)
    }

    fn verify_integrity_inner<C, P, F>(
        &self,
        mut progress: P,
        should_interrupt: &AtomicBool,
        deep_check: bool,
        integrity::Options {
            verify_mode,
            traversal,
            thread_limit,
            make_pack_lookup_cache,
        }: integrity::Options<F>,
    ) -> Result<integrity::Outcome<P>, crate::index::traverse::Error<integrity::Error>>
    where
        P: Progress,
        C: crate::cache::DecodeEntry,
        F: Fn() -> C + Send + Clone,
    {
        let parent = self.path.parent().expect("must be in a directory");

        let actual_index_checksum = self
            .verify_checksum(
                progress.add_child(format!("{}: checksum", self.path.display())),
                should_interrupt,
            )
            .map_err(integrity::Error::from)
            .map_err(crate::index::traverse::Error::Processor)?;

        if let Some(first_invalid) = crate::verify::fan(&self.fan) {
            return Err(crate::index::traverse::Error::Processor(integrity::Error::Fan {
                index: first_invalid,
            }));
        }

        if self.num_objects == 0 {
            return Err(crate::index::traverse::Error::Processor(integrity::Error::Empty));
        }

        let mut pack_traverse_statistics = Vec::new();

        let operation_start = Instant::now();
        let mut total_objects_checked = 0;
        let mut pack_ids_and_offsets = Vec::with_capacity(self.num_objects as usize);
        {
            let order_start = Instant::now();
            let mut progress = progress.add_child("checking oid order");
            progress.init(
                Some(self.num_objects as usize),
                git_features::progress::count("objects"),
            );

            for entry_index in 0..(self.num_objects - 1) {
                let lhs = self.oid_at_index(entry_index);
                let rhs = self.oid_at_index(entry_index + 1);

                if rhs.cmp(lhs) != Ordering::Greater {
                    return Err(crate::index::traverse::Error::Processor(integrity::Error::OutOfOrder {
                        index: entry_index,
                    }));
                }
                let (pack_id, _) = self.pack_id_and_pack_offset_at_index(entry_index);
                pack_ids_and_offsets.push((pack_id, entry_index));
                progress.inc();
            }
            {
                let entry_index = self.num_objects - 1;
                let (pack_id, _) = self.pack_id_and_pack_offset_at_index(entry_index);
                pack_ids_and_offsets.push((pack_id, entry_index));
            }
            // sort by pack-id to allow handling all indices matching a pack while its open.
            pack_ids_and_offsets.sort_by(|l, r| l.0.cmp(&r.0));
            progress.show_throughput(order_start);
        };

        progress.init(
            Some(self.num_indices as usize),
            git_features::progress::count("indices"),
        );

        let mut pack_ids_slice = pack_ids_and_offsets.as_slice();

        let offset_start = Instant::now();
        let mut offsets_progress = progress.add_child("verify object offsets");
        offsets_progress.init(
            Some(pack_ids_and_offsets.len()),
            git_features::progress::count("objects"),
        );

        for (pack_id, index_file_name) in self.index_names.iter().enumerate() {
            progress.set_name(index_file_name.display().to_string());
            progress.inc();

            let mut bundle = None;
            let index;
            let index_path = parent.join(index_file_name);
            let index = if deep_check {
                bundle = crate::Bundle::at(index_path, self.object_hash)
                    .map_err(integrity::Error::from)
                    .map_err(crate::index::traverse::Error::Processor)?
                    .into();
                bundle.as_ref().map(|b| &b.index).expect("just set")
            } else {
                index = Some(
                    crate::index::File::at(index_path, self.object_hash)
                        .map_err(|err| integrity::Error::BundleInit(crate::bundle::init::Error::Index(err)))
                        .map_err(crate::index::traverse::Error::Processor)?,
                );
                index.as_ref().expect("just set")
            };

            let slice_end = pack_ids_slice.partition_point(|e| e.0 == pack_id as crate::data::Id);
            let multi_index_entries_to_check = &pack_ids_slice[..slice_end];
            {
                pack_ids_slice = &pack_ids_slice[slice_end..];

                for entry_id in multi_index_entries_to_check.iter().map(|e| e.1) {
                    let oid = self.oid_at_index(entry_id);
                    let (_, expected_pack_offset) = self.pack_id_and_pack_offset_at_index(entry_id);
                    let entry_in_bundle_index = index.lookup(oid).ok_or_else(|| {
                        crate::index::traverse::Error::Processor(integrity::Error::OidNotFound { id: oid.to_owned() })
                    })?;
                    let actual_pack_offset = index.pack_offset_at_index(entry_in_bundle_index);
                    if actual_pack_offset != expected_pack_offset {
                        return Err(crate::index::traverse::Error::Processor(
                            integrity::Error::PackOffsetMismatch {
                                id: oid.to_owned(),
                                expected_pack_offset,
                                actual_pack_offset,
                            },
                        ));
                    }
                    offsets_progress.inc();
                }
                if should_interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(crate::index::traverse::Error::Processor(integrity::Error::Interrupted));
                }
            }

            total_objects_checked += multi_index_entries_to_check.len();

            progress.set_name("Validating");
            if let Some(bundle) = bundle {
                let progress = progress.add_child(index_file_name.display().to_string());
                let crate::bundle::verify::integrity::Outcome {
                    actual_index_checksum: _,
                    pack_traverse_outcome,
                    progress: _,
                } = bundle
                    .verify_integrity(
                        verify_mode,
                        traversal,
                        make_pack_lookup_cache.clone(),
                        thread_limit,
                        progress,
                        should_interrupt,
                    )
                    .map_err(|err| {
                        use crate::index::traverse::Error::*;
                        match err {
                            Processor(err) => Processor(integrity::Error::IndexIntegrity(err)),
                            VerifyChecksum(err) => VerifyChecksum(err),
                            Tree(err) => Tree(err),
                            TreeTraversal(err) => TreeTraversal(err),
                            PackDecode { id, offset, source } => PackDecode { id, offset, source },
                            PackMismatch { expected, actual } => PackMismatch { expected, actual },
                            PackObjectMismatch {
                                expected,
                                actual,
                                offset,
                                kind,
                            } => PackObjectMismatch {
                                expected,
                                actual,
                                offset,
                                kind,
                            },
                            Crc32Mismatch {
                                expected,
                                actual,
                                offset,
                                kind,
                            } => Crc32Mismatch {
                                expected,
                                actual,
                                offset,
                                kind,
                            },
                            Interrupted => Interrupted,
                        }
                    })?;
                pack_traverse_statistics.push(pack_traverse_outcome);
            }
        }

        assert_eq!(
            self.num_objects as usize, total_objects_checked,
            "BUG: our slicing should allow to visit all objects"
        );

        offsets_progress.show_throughput(offset_start);
        progress.show_throughput(operation_start);

        Ok(integrity::Outcome {
            actual_index_checksum,
            pack_traverse_statistics,
            progress,
        })
    }
}