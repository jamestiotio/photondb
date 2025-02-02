use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Instant,
};

use log::{debug, info, trace};

use crate::{
    env::Env,
    page_store::{
        page_file::{FileId, MapFileBuilder, PartialFileBuilder},
        page_table::PageTable,
        stats::{AtomicJobStats, AtomicWritebufStats},
        strategy::ReclaimPickStrategy,
        version::{DeltaVersion, VersionOwner, VersionUpdateReason},
        Error, FileInfo, Guard, Manifest, MapFileInfo, NewFile, Options, PageFiles, Result,
        StrategyBuilder, StreamEdit, Version, VersionEdit,
    },
    util::shutdown::{with_shutdown, Shutdown},
};

/// Rewrites pages to reclaim disk space.
pub(crate) trait RewritePage<E: Env>: Send + Sync + 'static {
    type Rewrite<'a>: Future<Output = Result<usize>> + Send + 'a
    where
        Self: 'a;

    /// Rewrites the corresponding page to reclaim the space it occupied.
    fn rewrite(&self, page_id: u64, guard: Guard<E>) -> Self::Rewrite<'_>;
}

pub(crate) struct ReclaimCtx<E, R>
where
    E: Env,
    R: RewritePage<E>,
{
    options: Options,
    shutdown: Shutdown,

    rewriter: R,
    strategy_builder: Box<dyn StrategyBuilder>,

    page_table: PageTable,
    page_files: Arc<PageFiles<E>>,
    version_owner: Arc<VersionOwner>,
    manifest: Arc<futures::lock::Mutex<Manifest<E>>>,

    next_map_file_id: u32,
    cleaned_files: HashSet<FileId>,
    orphan_page_files: HashSet<u32>,

    job_stats: Arc<AtomicJobStats>,
    writebuf_stats: Arc<AtomicWritebufStats>,
}

#[derive(Debug)]
struct ReclaimJobBuilder {
    enable: bool,
    target_file_base: usize,

    compound_files: HashSet<u32>,
    compound_size: usize,
    compact_files: HashSet<u32>,
    compact_size: usize,
}

#[derive(Debug)]
enum ReclaimJob {
    /// Rewrite page file.
    Rewrite(u32),
    /// Compound a set of page files into a new map file.
    Compound(HashSet<u32>),
    /// Compact a set of map files into a new map file.
    Compact(HashSet<u32>),
}

#[derive(Debug, Default)]
struct ReclaimProgress {
    // some options.
    target_space_amp: u64,
    space_used_high: u64,
    file_base_size: u64,

    used_space: u64,
    base_size: u64,
    additional_size: u64,
}

#[derive(Debug)]
enum ReclaimReason {
    None,
    HighSpaceUsage,
    LargeSpaceAmp,
}

#[derive(Debug, Default)]
struct CompactStats {
    num_active_pages: usize,
    input_size: usize,
    output_size: usize,
}

impl<E, R> ReclaimCtx<E, R>
where
    E: Env,
    R: RewritePage<E>,
{
    // FIXME: reduce number of arguments
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        options: Options,
        shutdown: Shutdown,
        rewriter: R,
        strategy_builder: Box<dyn StrategyBuilder>,
        page_table: PageTable,
        page_files: Arc<PageFiles<E>>,
        version_owner: Arc<VersionOwner>,
        manifest: Arc<futures::lock::Mutex<Manifest<E>>>,
        next_map_file_id: u32,
        orphan_page_files: HashSet<u32>,
        job_stat: Arc<AtomicJobStats>,
        writebuf_stats: Arc<AtomicWritebufStats>,
    ) -> Self {
        ReclaimCtx {
            options,
            shutdown,
            rewriter,
            strategy_builder,
            page_table,
            page_files,
            version_owner,
            manifest,
            next_map_file_id,
            cleaned_files: HashSet::default(),
            orphan_page_files,
            job_stats: job_stat,
            writebuf_stats,
        }
    }

    pub(crate) async fn run(mut self, mut version: Arc<Version>) {
        self.reclaim_orphan_page_files().await;
        loop {
            if !self.options.disable_space_reclaiming {
                self.reclaim(&version).await;
                version.reclaimed();
            }
            match with_shutdown(&mut self.shutdown, version.wait_next_version()).await {
                Some(next_version) => version = next_version.refresh().unwrap_or(next_version),
                None => break,
            }
        }
    }

    async fn reclaim(&mut self, version: &Arc<Version>) {
        // Reclaim deleted files in `cleaned_files`.
        let cleaned_files = std::mem::take(&mut self.cleaned_files);

        // Ignore the strategy, pick and reclaimate empty page files directly.
        let empty_files = self.pick_empty_page_files(version, &cleaned_files);
        self.rewrite_files(empty_files, version).await;

        let mut progress = ReclaimProgress::new(&self.options, version, &cleaned_files);
        progress.trace_log();
        if !progress.is_reclaimable() {
            return;
        }

        self.reclaim_files_by_strategy(&mut progress, version, &cleaned_files)
            .await;
    }

    fn pick_empty_page_files(
        &mut self,
        version: &Version,
        cleaned_files: &HashSet<FileId>,
    ) -> Vec<u32> {
        let mut empty_files = Vec::default();
        for (id, file) in version.page_files() {
            if cleaned_files.contains(&FileId::Page(*id)) {
                self.cleaned_files.insert(FileId::Page(*id));
                continue;
            }

            if file.is_empty() && file.get_map_file_id().is_none() {
                empty_files.push(*id);
            }
        }
        empty_files
    }

    async fn reclaim_orphan_page_files(&mut self) {
        let version = self.version_owner.current();
        let orphan_page_files = std::mem::take(&mut self.orphan_page_files);
        if orphan_page_files.is_empty() {
            return;
        }

        info!("reclaim orphan page files {:?}", orphan_page_files);
        for &file_id in &orphan_page_files {
            let reader = self
                .page_files
                .open_page_file_meta_reader(file_id)
                .await
                .unwrap();
            let dealloc_pages = reader.read_delete_pages().await.unwrap();
            self.rewrite_dealloc_pages(file_id, &version, &dealloc_pages)
                .await
                .unwrap();
        }

        let edit = make_orphan_page_file_edit(&orphan_page_files);
        let mut manifest = self.manifest.lock().await;
        let version = self.version_owner.current();
        manifest
            .record_version_edit(edit, || super::version_snapshot(&version))
            .await
            .unwrap();
        let mut delta = DeltaVersion::from(version.as_ref());
        delta.reason = VersionUpdateReason::Compact;
        delta.page_files.drain_filter(|id, info| {
            info.get_map_file_id().is_none() && orphan_page_files.contains(id)
        });
        delta.obsoleted_page_files = orphan_page_files;

        // Safety: the mutable reference of [`Manifest`] is hold.
        unsafe { self.version_owner.install(delta) };
    }

    async fn reclaim_files_by_strategy(
        &mut self,
        progress: &mut ReclaimProgress,
        version: &Arc<Version>,
        cleaned_files: &HashSet<FileId>,
    ) {
        let mut strategy = self.build_strategy(version, cleaned_files);
        let mut builder = ReclaimJobBuilder::new(
            self.options.separate_hot_cold_files,
            self.options.file_base_size,
        );
        while let Some((file, active_size)) = strategy.apply() {
            if let Some(job) = builder.add(file, active_size) {
                match job {
                    ReclaimJob::Rewrite(file_id) => {
                        if let Err(err) = self
                            .rewrite_file_with_progress(Some(progress), file_id, version)
                            .await
                        {
                            todo!("reclaim files: {err:?}");
                        }
                    }
                    ReclaimJob::Compound(victims) => {
                        self.reclaim_page_files(progress, version, victims).await;
                    }
                    ReclaimJob::Compact(victims) => {
                        self.reclaim_map_files(progress, version, victims).await;
                    }
                }
            }

            if self.shutdown.is_terminated()
                || version.has_next_version()
                || !progress.is_reclaimable()
            {
                break;
            }
        }
    }

    /// Reclaim page files by compounding victims into a new map file, and
    /// install new version.
    async fn reclaim_page_files(
        &mut self,
        progress: &mut ReclaimProgress,
        version: &Arc<Version>,
        victims: HashSet<u32>,
    ) {
        let file_id = self.next_map_file_id;
        self.next_map_file_id += 1;

        let file_infos = version.page_files();
        let (virtual_page_files, map_file, obsoleted_files, dealloc_page_map) = self
            .compound_page_files(progress, file_id, file_infos, victims)
            .await
            .unwrap();

        let edit = make_compound_version_edit(&map_file, &obsoleted_files);
        let mut manifest = self.manifest.lock().await;
        let version = self.version_owner.current();
        manifest
            .record_version_edit(edit, || super::version_snapshot(&version))
            .await
            .unwrap();

        let mut delta = DeltaVersion::from(version.as_ref());
        delta.reason = VersionUpdateReason::Compact;
        delta.map_files.insert(file_id, map_file);
        // FIXME: need remove empty infos if it is not contained in virtual_info.
        delta.page_files.extend(virtual_page_files.into_iter());
        delta.obsoleted_page_files = obsoleted_files.into_iter().collect();
        // Safety: the mutable reference of [`Manifest`] is hold.
        unsafe { self.version_owner.install(delta) };

        // Rewrite dealloc pages for compounded page files.
        //
        // It's recoverable, see `reclaim_orphan_page_files` for details.
        drop(manifest);
        for (id, dealloc_pages) in dealloc_page_map {
            self.rewrite_dealloc_pages(id, &version, &dealloc_pages)
                .await
                .unwrap();
        }
    }

    async fn reclaim_map_files(
        &mut self,
        progress: &mut ReclaimProgress,
        version: &Arc<Version>,
        victims: HashSet<u32>,
    ) {
        let file_id = self.next_map_file_id;
        self.next_map_file_id += 1;

        let map_files = version.map_files();
        let page_files = version.page_files();
        let (virtual_infos, file_info) = self
            .compact_map_files(progress, file_id, map_files, page_files, &victims)
            .await
            .unwrap();

        // All input are obsoleted, since it doesn't relocate pages.
        let edit = make_compact_version_edit(&file_info, &victims);
        let mut manifest = self.manifest.lock().await;
        let version = self.version_owner.current();
        manifest
            .record_version_edit(edit, || super::version_snapshot(&version))
            .await
            .unwrap();

        let mut delta = DeltaVersion::from(version.as_ref());
        delta.reason = VersionUpdateReason::Compact;
        delta.map_files.retain(|id, _| !victims.contains(id));
        delta.map_files.insert(file_id, file_info);
        // FIXME: need remove empty infos if it is not contained in virtual_info.
        delta.page_files.extend(virtual_infos.into_iter());
        delta.obsoleted_map_files = victims.into_iter().collect();
        // Safety: the mutable reference of [`Manifest`] is hold.
        unsafe { self.version_owner.install(delta) };
    }

    async fn rewrite_files(&mut self, files: Vec<u32>, version: &Arc<Version>) {
        for file_id in files {
            if self.shutdown.is_terminated() || version.has_next_version() {
                break;
            }

            if let Err(err) = self.rewrite_file(file_id, version).await {
                todo!("rewrite files: {err:?}");
            }
        }
    }

    #[inline]
    async fn rewrite_file(&mut self, file_id: u32, version: &Arc<Version>) -> Result<()> {
        self.rewrite_file_with_progress(None, file_id, version)
            .await
    }

    async fn rewrite_file_with_progress(
        &mut self,
        progress: Option<&mut ReclaimProgress>,
        file_id: u32,
        version: &Arc<Version>,
    ) -> Result<()> {
        if self.cleaned_files.contains(&FileId::Page(file_id)) {
            // This file has been rewritten.
            return Ok(());
        }

        let file = version
            .page_files()
            .get(&file_id)
            .expect("File must exists");
        self.rewrite_file_impl(file, version).await?;
        if let Some(progress) = progress {
            progress.track_page_file(file);
        }
        self.cleaned_files.insert(FileId::Page(file_id));
        Ok(())
    }

    async fn rewrite_file_impl(&self, file: &FileInfo, version: &Arc<Version>) -> Result<()> {
        let start_at = Instant::now();
        let file_id = file.get_file_id();
        let reader = self.page_files.open_page_file_meta_reader(file_id).await?;
        let page_table = reader.read_page_table().await?;
        let dealloc_pages = reader.read_delete_pages().await?;
        self.job_stats
            .read_file_bytes
            .add(reader.into_inner().total_read_bytes());

        let total_rewrite_pages = self
            .rewrite_active_pages(file, version, &page_table)
            .await?;
        let total_dealloc_pages = self
            .rewrite_dealloc_pages(file_id, version, &dealloc_pages)
            .await?;

        let effective_size = file.effective_size();
        let file_size = file.file_size();
        let free_size = file_size - effective_size;
        let free_ratio = free_size as f64 / file_size as f64;
        let elapsed = start_at.elapsed().as_micros();
        info!(
            "Rewrite file {file_id} with {total_rewrite_pages} active pages, \
                {total_dealloc_pages} dealloc pages, relocate {effective_size} bytes, \
                free {free_size} bytes, free ratio {free_ratio:.4}, latest {elapsed} microseconds",
        );

        Ok(())
    }

    async fn rewrite_active_pages(
        &self,
        file: &FileInfo,
        version: &Arc<Version>,
        page_table: &BTreeMap<u64, u64>,
    ) -> Result<usize> {
        let mut total_rewrite_pages = 0;
        let mut rewrite_pages = HashSet::new();
        let mut rewrite_size = 0;
        for page_addr in file.iter() {
            let page_id = page_table
                .get(&page_addr)
                .cloned()
                .expect("Page mapping must exists in page table");
            total_rewrite_pages += 1;
            if rewrite_pages.contains(&page_id) {
                continue;
            }
            rewrite_pages.insert(page_id);
            let guard = Guard::new(
                version.clone(),
                self.page_table.clone(),
                self.page_files.clone(),
                self.writebuf_stats.clone(),
            );
            rewrite_size += self.rewriter.rewrite(page_id, guard).await?;
        }
        self.job_stats
            .rewrite_input_bytes
            .add(file.total_page_size() as u64);
        self.job_stats.rewrite_bytes.add(rewrite_size as u64);
        Ok(total_rewrite_pages)
    }

    async fn rewrite_dealloc_pages(
        &self,
        file_id: u32,
        version: &Arc<Version>,
        dealloc_pages: &[u64],
    ) -> Result<usize> {
        let active_files = version.page_files();
        let mut total_rewrite_pages = 0;
        let mut cached_pages = Vec::with_capacity(128);
        for page_addr in dealloc_pages {
            let file_id = (page_addr >> 32) as u32;
            if !active_files.contains_key(&file_id) {
                continue;
            }

            if cached_pages.len() == 128 {
                self.rewrite_dealloc_pages_chunk(None, version, &cached_pages)
                    .await?;
                cached_pages.clear();
            }
            cached_pages.push(*page_addr);
            total_rewrite_pages += 1;
        }

        // Ensure the `file_id` is recorded in write buffer.
        if total_rewrite_pages != 0 {
            assert!(!cached_pages.is_empty());
            self.rewrite_dealloc_pages_chunk(Some(file_id), version, &cached_pages)
                .await?;
        }

        Ok(total_rewrite_pages)
    }

    async fn rewrite_dealloc_pages_chunk(
        &self,
        file_id: Option<u32>,
        version: &Arc<Version>,
        pages: &[u64],
    ) -> Result<()> {
        loop {
            let guard = Guard::new(
                version.clone(),
                self.page_table.clone(),
                self.page_files.clone(),
                self.writebuf_stats.clone(),
            );
            let txn = guard.begin().await;
            match txn.dealloc_pages(file_id, pages).await {
                Ok(()) => return Ok(()),
                Err(Error::Again) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn build_strategy(
        &mut self,
        version: &Version,
        cleaned_files: &HashSet<FileId>,
    ) -> Box<dyn ReclaimPickStrategy> {
        let now = version.now();
        let files = version.page_files();
        let mut strategy = self.strategy_builder.build(now);
        for (&id, file) in files {
            if cleaned_files.contains(&FileId::Page(id)) {
                self.cleaned_files.insert(FileId::Page(id));
                continue;
            }

            // Skip empty or virtual page file.
            if !file.is_empty() && file.get_map_file_id().is_none() {
                strategy.collect_page_file(file);
            }
        }
        let map_files = version.map_files();
        for (&id, file) in map_files {
            if cleaned_files.contains(&FileId::Map(id)) {
                self.cleaned_files.insert(FileId::Map(id));
                continue;
            }

            strategy.collect_map_file(files, file);
        }
        strategy
    }

    /// Compound a set of page files into a map file
    ///
    /// NOTE: We don't mix page file and map file in compounding, because they
    /// have different age (update frequency).
    async fn compound_page_files(
        &mut self,
        progress: &mut ReclaimProgress,
        new_file_id: u32,
        file_infos: &HashMap<u32, FileInfo>,
        victims: HashSet<u32>,
    ) -> Result<(
        HashMap<u32, FileInfo>,
        MapFileInfo,
        Vec<u32>,
        HashMap<u32, Vec<u64>>,
    )> {
        let start_at = Instant::now();
        let mut num_active_pages = 0;
        let mut num_dealloc_pages = 0;
        let mut input_size = 0;
        let mut output_size = 0;
        let mut builder = self
            .page_files
            .new_map_file_builder(
                new_file_id,
                self.options.compression_on_cold_compact,
                self.options.page_checksum_type,
            )
            .await?;
        let mut obsoleted_files = vec![];
        let mut dealloc_page_map = HashMap::default();
        let mut victims = victims.into_iter().collect::<Vec<_>>();
        victims.sort_unstable();
        let mut up2_sum = 0;
        for &id in &victims {
            let dealloc_pages;

            // Compound page file into map file
            let file_builder = builder.add_file(id);
            let file_info = file_infos.get(&id).expect("Victims must exists");
            up2_sum += file_info.up2();
            input_size += file_info.file_size();
            output_size += file_info.effective_size();
            num_active_pages += file_info.num_active_pages();
            (builder, dealloc_pages) = self
                .compound_partial_page_file(file_builder, file_info)
                .await?;
            num_dealloc_pages += dealloc_pages.len();

            // .. and rewrite dealloc pages if exists
            if dealloc_pages.is_empty() {
                obsoleted_files.push(id);
            } else {
                dealloc_page_map.insert(id, dealloc_pages);
            }
            progress.track_page_file(file_info);
            self.cleaned_files.insert(FileId::Page(id));
        }

        // When we include the page in a new segment that contains re-written pages from
        // other segments, the value for up2 for the new segment is the average up2 for
        // all pages written to it.
        let up2 = up2_sum / (victims.len() as u32);
        let (virtual_infos, file_info) = builder.finish(up2).await?;

        self.job_stats.compact_input_bytes.add(input_size as u64);
        self.job_stats.compact_write_bytes.add(output_size as u64);
        let elapsed = start_at.elapsed().as_micros();
        let free_size = input_size.saturating_sub(output_size);
        let free_ratio = free_size as f64 / input_size as f64;
        info!(
            "Compound page files {victims:?} into map file {new_file_id} \
                with {num_active_pages} active pages, \
                {num_dealloc_pages} dealloc pages, \
                relocate {output_size} bytes, \
                free {free_size} bytes, free ratio {free_ratio:.4}, \
                latest {elapsed} microseconds",
        );

        Ok((virtual_infos, file_info, obsoleted_files, dealloc_page_map))
    }

    /// Write all active pages of the corresponding page file into a map file
    /// (not include dealloc pages).
    async fn compound_partial_page_file<'a>(
        &self,
        mut builder: PartialFileBuilder<'a, E>,
        file_info: &FileInfo,
    ) -> Result<(MapFileBuilder<'a, E>, Vec<u64>)> {
        let file_id = file_info.get_file_id();
        debug!("compound partial page file {file_id}");
        let reader = self
            .page_files
            .open_page_file_meta_reader(file_id)
            .await
            .unwrap();
        let page_table = reader.read_page_table().await.unwrap();
        let dealloc_pages = reader.read_delete_pages().await.unwrap();
        let reader = reader.into_inner();
        let mut page = vec![];
        for page_addr in file_info.iter() {
            let page_id = page_table
                .get(&page_addr)
                .cloned()
                .expect("Page mapping must exists in page table");
            let handle = file_info
                .get_page_handle(page_addr)
                .expect("Handle of active page must exists");
            let page_size = handle.size as usize;
            if page.len() < page_size {
                page.resize(page_size, 0u8);
            }
            page.truncate(page_size);
            self.page_files
                .read_file_page_from_reader(reader.clone(), file_info.meta(), handle, &mut page)
                .await
                .unwrap();
            builder.add_page(page_id, page_addr, &page).await.unwrap();
        }
        self.job_stats
            .read_file_bytes
            .add(reader.total_read_bytes());
        let builder = builder.finish().await.unwrap();
        Ok((builder, dealloc_pages))
    }

    /// Compact a set of map files into a new map file, and release mark the
    /// compacted files as obsoleted to reclaim space.
    async fn compact_map_files(
        &mut self,
        progress: &mut ReclaimProgress,
        new_file_id: u32,
        map_files: &HashMap<u32, MapFileInfo>,
        page_files: &HashMap<u32, FileInfo>,
        victims: &HashSet<u32>,
    ) -> Result<(HashMap<u32, FileInfo>, MapFileInfo)> {
        let start_at = Instant::now();
        let mut builder = self
            .page_files
            .new_map_file_builder(
                new_file_id,
                self.options.compression_on_cold_compact,
                self.options.page_checksum_type,
            )
            .await?;
        let mut victims = victims.iter().cloned().collect::<Vec<_>>();
        victims.sort_unstable();
        let mut stats = CompactStats::default();
        let mut up2_sum = 0;
        for &id in &victims {
            let info = map_files.get(&id).expect("Must exists");
            up2_sum += info.up2();
            builder = self
                .compact_map_file(builder, &mut stats, info, page_files)
                .await?;
            self.cleaned_files.insert(FileId::Map(id));
            progress.track_map_file(info, page_files);
        }

        // When we include the page in a new segment that contains re-written pages from
        // other segments, the value for up2 for the new segment is the average up2 for
        // all pages written to it.
        let up2 = up2_sum / (victims.len() as u32);
        let (virtual_infos, file_info) = builder.finish(up2).await?;

        let elapsed = start_at.elapsed().as_micros();
        let CompactStats {
            num_active_pages,
            input_size,
            output_size,
        } = stats;
        self.job_stats.compact_input_bytes.add(input_size as u64);
        self.job_stats.compact_write_bytes.add(output_size as u64);
        let free_size = input_size.saturating_sub(output_size);
        let free_ratio = (free_size as f64) / (input_size as f64);
        info!(
            "Compact map files {victims:?} into a new map file {new_file_id} \
                    with {num_active_pages} active pages, \
                    relocate {output_size} bytes, \
                    free {free_size} bytes, free ratio {free_ratio:.4}, \
                    latest {elapsed} microseconds"
        );

        Ok((virtual_infos, file_info))
    }

    async fn compact_map_file<'a>(
        &self,
        mut builder: MapFileBuilder<'a, E>,
        stats: &mut CompactStats,
        file_info: &MapFileInfo,
        page_files: &HashMap<u32, FileInfo>,
    ) -> Result<MapFileBuilder<'a, E>> {
        let file_id = file_info.file_id();
        debug!("compact file {file_id}");
        let file_meta = self.page_files.read_map_file_meta(file_id).await?;
        let reader = self
            .page_files
            .open_page_reader(FileId::Map(file_id), 4096)
            .await?;
        let mut target_files = file_meta.file_meta_map.keys().cloned().collect::<Vec<_>>();
        target_files.sort_unstable();
        let mut page = vec![];
        for id in target_files {
            let info = page_files.get(&id).expect("Must exists");
            stats.collect(info);
            let page_table = file_meta.page_tables.get(&id).expect("Must exists");
            let mut partial_builder = builder.add_file(id);
            for page_addr in info.iter() {
                let page_id = *page_table.get(&page_addr).expect("Must exists");
                let handle = info.get_page_handle(page_addr).expect("Must exists");

                let page_size = handle.size as usize;
                if page.len() < page_size {
                    page.resize(page_size, 0u8);
                }
                page.truncate(page_size);
                self.page_files
                    .read_file_page_from_reader(reader.clone(), info.meta(), handle, &mut page)
                    .await?;
                partial_builder.add_page(page_id, page_addr, &page).await?;
            }
            builder = partial_builder.finish().await?;
        }
        self.job_stats
            .read_file_bytes
            .add(reader.total_read_bytes());
        Ok(builder)
    }
}

impl ReclaimJobBuilder {
    fn new(enable: bool, target_file_base: usize) -> ReclaimJobBuilder {
        ReclaimJobBuilder {
            enable,
            target_file_base,

            compact_files: HashSet::default(),
            compact_size: 0,
            compound_files: HashSet::default(),
            compound_size: 0,
        }
    }

    fn add(&mut self, file: FileId, active_size: usize) -> Option<ReclaimJob> {
        // A switch disable map files before we have full supports.
        if !self.enable {
            let FileId::Page(file_id) = file else { panic!("not implemented") };
            return Some(ReclaimJob::Rewrite(file_id));
        }

        match file {
            FileId::Page(file_id) => {
                // Rewrite small page files (16KB <=) directly.
                if active_size < 16 << 10 {
                    return Some(ReclaimJob::Rewrite(file_id));
                }

                self.compound_size += active_size;
                self.compound_files.insert(file_id);
                if self.compound_size >= self.target_file_base {
                    self.compound_size = 0;
                    return Some(ReclaimJob::Compound(std::mem::take(
                        &mut self.compound_files,
                    )));
                }
            }
            FileId::Map(file_id) => {
                self.compact_size += active_size;
                self.compact_files.insert(file_id);
                if self.compact_size >= self.target_file_base {
                    self.compact_size = 0;
                    return Some(ReclaimJob::Compact(std::mem::take(&mut self.compact_files)));
                }
            }
        }
        None
    }
}

impl ReclaimProgress {
    fn new(
        option: &Options,
        version: &Version,
        cleaned_files: &HashSet<FileId>,
    ) -> ReclaimProgress {
        let target_space_amp = option.max_space_amplification_percent as u64;
        let space_used_high = option.space_used_high;
        let file_base_size = option.file_base_size as u64;
        let used_space =
            compute_used_space(version.page_files(), version.map_files(), cleaned_files);
        let base_size = compute_base_size(version.page_files(), cleaned_files);
        let additional_size = used_space.saturating_sub(base_size);
        ReclaimProgress {
            target_space_amp,
            space_used_high,
            file_base_size,
            used_space,
            base_size,
            additional_size,
        }
    }

    fn track_page_file(&mut self, file: &FileInfo) {
        assert!(file.get_map_file_id().is_none());
        self.used_space = self.used_space.saturating_sub(file.file_size() as u64);
        self.base_size = self.base_size.saturating_sub(file.effective_size() as u64);
        self.additional_size = self.used_space.saturating_sub(self.base_size);
    }

    fn track_map_file(&mut self, file: &MapFileInfo, page_files: &HashMap<u32, FileInfo>) {
        self.used_space = self.used_space.saturating_sub(file.file_size() as u64);
        let effective_size = file
            .meta()
            .page_files()
            .keys()
            .map(|id| {
                page_files
                    .get(id)
                    .map(FileInfo::effective_size)
                    .unwrap_or_default()
            })
            .sum::<usize>() as u64;
        self.base_size = self.base_size.saturating_sub(effective_size);
        self.additional_size = self.used_space.saturating_sub(self.base_size);
    }

    fn reclaim_reason(&self) -> ReclaimReason {
        // If space usage exceeds high watermark,
        if self.space_used_high < self.used_space
            // .. and enough space for reclaiming.
            && 2 * self.file_base_size < self.additional_size
        {
            ReclaimReason::HighSpaceUsage
        } else if 0 < self.additional_size
            && self.target_space_amp * self.base_size <= self.additional_size * 100
        {
            ReclaimReason::LargeSpaceAmp
        } else {
            ReclaimReason::None
        }
    }

    fn is_reclaimable(&self) -> bool {
        match self.reclaim_reason() {
            ReclaimReason::HighSpaceUsage | ReclaimReason::LargeSpaceAmp => true,
            ReclaimReason::None => false,
        }
    }

    fn trace_log(&self) {
        let space_amp = (self.additional_size as f64) / (self.base_size as f64);
        match self.reclaim_reason() {
            ReclaimReason::HighSpaceUsage => {
                trace!(
                    "db is reclaimable: space used {} exceeds water mark {}, base size {}, amp {:.4}",
                    self.used_space,
                    self.space_used_high,
                    self.base_size,
                    space_amp
                );
            }
            ReclaimReason::LargeSpaceAmp => {
                trace!(
                    "db is reclaimable: space amplification {:.4} exceeds target {}, base size {}, used space {}",
                    space_amp, self.target_space_amp, self.base_size, self.used_space
                );
            }
            ReclaimReason::None => {
                trace!(
                    "db is not reclaimable, base size {}, additional size {}, used space {}, used high {}, space amp {:.4}",
                    self.base_size,
                    self.additional_size,
                    self.used_space,
                    self.space_used_high,
                    space_amp
                );
            }
        }
    }
}

impl CompactStats {
    fn collect(&mut self, info: &FileInfo) {
        self.num_active_pages += info.num_active_pages();
        self.input_size += info.total_page_size();
        self.output_size += info.effective_size();
    }
}

/// Wait until the running reclaiming progress to finish.
pub(crate) async fn wait_for_reclaiming(options: &Options, mut version: Arc<Version>) {
    if options.disable_space_reclaiming {
        return;
    }

    loop {
        let progress = ReclaimProgress::new(options, &version, &HashSet::default());
        progress.trace_log();
        if progress.is_reclaimable() {
            version.wait_for_reclaiming().await;
            if let Some(next_version) = version.try_next() {
                version = next_version;
                continue;
            }
        }
        break;
    }
}

fn compute_base_size(page_files: &HashMap<u32, FileInfo>, cleaned_files: &HashSet<FileId>) -> u64 {
    // skip files that are already being cleaned.
    let allow_file = |info: &&FileInfo| {
        !info.is_empty()
            && if let Some(map_file_id) = info.get_map_file_id() {
                !cleaned_files.contains(&FileId::Map(map_file_id))
            } else {
                !cleaned_files.contains(&FileId::Page(info.get_file_id()))
            }
    };
    page_files
        .values()
        .filter(allow_file)
        .map(FileInfo::effective_size)
        .sum::<usize>() as u64
}

fn compute_used_space(
    page_files: &HashMap<u32, FileInfo>,
    map_files: &HashMap<u32, MapFileInfo>,
    cleaned_files: &HashSet<FileId>,
) -> u64 {
    // skip files that are already being cleaned.
    let allow_file = |info: &&FileInfo| {
        !info.is_empty()
            && info.get_map_file_id().is_none()
            && !cleaned_files.contains(&FileId::Page(info.get_file_id()))
    };
    let page_file_size = page_files
        .values()
        .filter(allow_file)
        .map(FileInfo::file_size)
        .sum::<usize>() as u64;
    let map_file_size = map_files
        .values()
        .filter(|info| !cleaned_files.contains(&FileId::Map(info.file_id())))
        .map(MapFileInfo::file_size)
        .sum::<usize>() as u64;
    map_file_size + page_file_size
}

fn make_compound_version_edit(file_info: &MapFileInfo, obsoleted_files: &[u32]) -> VersionEdit {
    let new_files = vec![NewFile::from(file_info)];
    VersionEdit {
        map_stream: Some(StreamEdit {
            new_files,
            deleted_files: vec![],
        }),
        page_stream: Some(StreamEdit {
            new_files: vec![],
            deleted_files: obsoleted_files.to_owned(),
        }),
    }
}

fn make_compact_version_edit(
    file_info: &MapFileInfo,
    obsoleted_files: &HashSet<u32>,
) -> VersionEdit {
    let deleted_files = obsoleted_files.iter().cloned().collect::<Vec<_>>();
    let new_files = vec![NewFile::from(file_info)];
    VersionEdit {
        map_stream: Some(StreamEdit {
            new_files,
            deleted_files,
        }),
        page_stream: None,
    }
}

fn make_orphan_page_file_edit(obsoleted_files: &HashSet<u32>) -> VersionEdit {
    let deleted_files = obsoleted_files.iter().cloned().collect::<Vec<_>>();
    VersionEdit {
        map_stream: None,
        page_stream: Some(StreamEdit {
            new_files: vec![],
            deleted_files,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        path::Path,
        sync::Mutex,
    };

    use tempdir::TempDir;

    use super::*;
    use crate::{
        env::Photon,
        page_store::{
            page_file::Compression, version::DeltaVersion, ChecksumType,
            MinDeclineRateStrategyBuilder, RecordRef,
        },
        util::shutdown::ShutdownNotifier,
    };

    #[derive(Clone, Default)]
    struct PageRewriter {
        values: Arc<Mutex<Vec<u64>>>,
    }

    impl PageRewriter {
        fn pages(&self) -> Vec<u64> {
            self.values.lock().unwrap().clone()
        }
    }

    impl RewritePage<Photon> for PageRewriter {
        type Rewrite<'a> = impl Future<Output = Result<usize, Error>> + Send + 'a
        where
            Self: 'a;

        fn rewrite(&self, id: u64, _guard: Guard<Photon>) -> Self::Rewrite<'_> {
            self.values.lock().unwrap().push(id);
            async { Ok(0) }
        }
    }

    async fn build_page_file(
        page_files: &PageFiles<Photon>,
        file_id: u32,
        pages: &[(u64, u64)],
        dealloc_pages: &[u64],
    ) -> FileInfo {
        let mut builder = page_files
            .new_page_file_builder(file_id, Compression::ZSTD)
            .await
            .unwrap();
        for (page_id, page_addr) in pages {
            builder.add_page(*page_id, *page_addr, &[0]).await.unwrap();
        }
        builder.add_delete_pages(dealloc_pages);
        builder.finish().await.unwrap()
    }

    async fn build_reclaim_ctx(
        dir: &Path,
        rewriter: PageRewriter,
    ) -> ReclaimCtx<Photon, PageRewriter> {
        let notifier = ShutdownNotifier::new();
        let shutdown = notifier.subscribe();
        let strategy_builder = Box::new(MinDeclineRateStrategyBuilder);
        let options = Options {
            cache_capacity: 2 << 10,
            ..Default::default()
        };
        let manifest = Arc::new(futures::lock::Mutex::new(
            Manifest::open(Photon, &dir).await.unwrap(),
        ));
        let version_owner = Arc::new(VersionOwner::new(Version::new(
            1 << 20,
            1,
            10,
            DeltaVersion::default(),
        )));
        let page_files = Arc::new(PageFiles::new(Photon, dir, &options).await);
        let orphan_page_files = HashSet::default();
        ReclaimCtx {
            options,
            shutdown,
            rewriter,
            strategy_builder,
            page_table: PageTable::default(),
            page_files,
            manifest,
            version_owner,
            cleaned_files: HashSet::default(),
            next_map_file_id: 1,
            orphan_page_files,
            job_stats: Arc::default(),
            writebuf_stats: Default::default(),
        }
    }

    #[photonio::test]
    async fn reclaim_rewrite_page() {
        let root = TempDir::new("reclaim_rewrite_page").unwrap();
        let root = root.into_path();

        let rewriter = PageRewriter::default();
        let ctx = build_reclaim_ctx(&root, rewriter.clone()).await;
        let mut file_info = build_page_file(
            &ctx.page_files,
            2,
            &[
                (1, pa(2, 16)),
                (2, pa(2, 32)),
                (3, pa(2, 64)),
                (4, pa(2, 128)),
            ],
            &[301, 302, 303],
        )
        .await;
        assert!(file_info.deactivate_page(3, pa(2, 32)));

        let mut files = HashMap::new();
        files.insert(2, file_info.clone());
        let delta = DeltaVersion {
            page_files: files,
            ..Default::default()
        };
        let version = Arc::new(Version::new(1 << 20, 3, 8, delta));

        ctx.rewrite_file_impl(&file_info, &version).await.unwrap();
        assert_eq!(rewriter.pages(), vec![1, 3, 4]); // page_id 2 is deallocated.

        let buf = version.min_write_buffer();
        buf.seal().unwrap();
        let dealloc_pages = HashSet::from([301, 302, 303]);
        for (_, header, record_ref) in buf.iter() {
            match record_ref {
                RecordRef::DeallocPages(pages) => {
                    assert_eq!(header.former_file_id(), 2);
                    for page in pages {
                        assert!(dealloc_pages.contains(&page));
                    }
                }
                RecordRef::Page(_page) => unreachable!(),
            }
        }
    }

    fn pa(file_id: u32, offset: u32) -> u64 {
        ((file_id as u64) << 32) | (offset as u64)
    }

    #[photonio::test]
    async fn compound_page_files() {
        let root = TempDir::new("compound_page_files").unwrap();
        let root = root.into_path();

        let rewriter = PageRewriter::default();
        let mut ctx = build_reclaim_ctx(&root, rewriter.clone()).await;
        let file_id_1 = 2;
        let file_id_2 = 3;
        let mut file_info_1 = build_page_file(
            &ctx.page_files,
            file_id_1,
            &[
                (1, pa(file_id_1, 16)),
                (2, pa(file_id_1, 32)),
                (3, pa(file_id_1, 64)),
                (4, pa(file_id_1, 128)),
            ],
            &[301, 302, 303],
        )
        .await;
        assert!(file_info_1.deactivate_page(3, pa(file_id_1, 32)));

        let file_info_2 = build_page_file(
            &ctx.page_files,
            file_id_2,
            &[
                (11, pa(file_id_2, 16)),
                (12, pa(file_id_2, 32)),
                (13, pa(file_id_2, 64)),
                (14, pa(file_id_2, 128)),
            ],
            &[301, 302, 303],
        )
        .await;

        let mut files = HashMap::new();
        files.insert(file_id_1, file_info_1.clone());
        files.insert(file_id_2, file_info_2.clone());

        let version = ctx.version_owner.current();
        let victims = files.keys().cloned().collect::<HashSet<_>>();
        let mut progress = ReclaimProgress::new(&ctx.options, &version, &HashSet::default());
        let (new_files, map_file, _, dealloc_page_map) = ctx
            .compound_page_files(&mut progress, 1, &files, victims)
            .await
            .unwrap();
        for (id, pages) in dealloc_page_map {
            ctx.rewrite_dealloc_pages(id, &version, &pages)
                .await
                .unwrap();
        }
        assert_eq!(map_file.meta().num_page_files(), 2);
        assert!(new_files.contains_key(&file_id_1));
        assert!(new_files.contains_key(&file_id_2));
        assert!(new_files
            .get(&file_id_1)
            .unwrap()
            .is_page_active(pa(file_id_1, 16)));
        assert!(!new_files
            .get(&file_id_1)
            .unwrap()
            .is_page_active(pa(file_id_1, 32)));
        assert!(new_files
            .get(&file_id_1)
            .unwrap()
            .get_page_handle(pa(file_id_1, 32))
            .is_none());
        assert!(new_files
            .get(&file_id_2)
            .unwrap()
            .is_page_active(pa(file_id_2, 32)));
    }

    async fn build_map_file(
        page_files: &PageFiles<Photon>,
        file_id: u32,
        pages: HashMap<u32, Vec<(u64, u64)>>,
    ) -> (HashMap<u32, FileInfo>, MapFileInfo) {
        let mut builder = page_files
            .new_map_file_builder(file_id, Compression::ZSTD, ChecksumType::CRC32)
            .await
            .unwrap();
        for (id, pages) in pages {
            let mut file_builder = builder.add_file(id);
            for (page_id, page_addr) in pages {
                file_builder
                    .add_page(page_id, page_addr, &[0])
                    .await
                    .unwrap();
            }
            builder = file_builder.finish().await.unwrap();
        }
        builder.finish(file_id).await.unwrap()
    }

    #[photonio::test]
    async fn map_files_compacting() {
        let root = TempDir::new("compact_map_files").unwrap();
        let root = root.into_path();

        let rewriter = PageRewriter::default();
        let mut ctx = build_reclaim_ctx(&root, rewriter.clone()).await;

        let (f1, f2, f3, f4) = (1, 2, 3, 4);
        let (m1, m2, m3) = (1, 2, 3);
        let mut pages = HashMap::new();
        pages.insert(f1, vec![(1, pa(f1, 16)), (2, pa(f1, 32)), (3, pa(f1, 64))]);
        pages.insert(f2, vec![(4, pa(f2, 16)), (5, pa(f2, 32)), (6, pa(f2, 64))]);
        let (virtual_infos, m1_info) = build_map_file(&ctx.page_files, m1, pages).await;
        let mut page_files = virtual_infos;

        let mut pages = HashMap::new();
        pages.insert(f3, vec![(7, pa(f3, 16)), (8, pa(f3, 32)), (9, pa(f3, 64))]);
        pages.insert(f4, vec![(1, pa(f4, 16)), (2, pa(f4, 32)), (3, pa(f4, 64))]);
        let (virtual_infos, m2_info) = build_map_file(&ctx.page_files, m2, pages).await;
        page_files.extend(virtual_infos.into_iter());

        let mut map_files = HashMap::new();
        map_files.insert(m1, m1_info);
        map_files.insert(m2, m2_info);
        let victims = HashSet::from_iter(vec![m1, m2].into_iter());
        let version = ctx.version_owner.current();
        let mut progress = ReclaimProgress::new(&ctx.options, &version, &HashSet::default());
        let (virtual_infos, m3_info) = ctx
            .compact_map_files(&mut progress, m3, &map_files, &page_files, &victims)
            .await
            .unwrap();

        assert!(virtual_infos.contains_key(&f1));
        assert!(virtual_infos.contains_key(&f2));
        assert!(virtual_infos.contains_key(&f3));
        assert!(virtual_infos.contains_key(&f4));

        let f1_info = virtual_infos.get(&f1).unwrap();
        assert!(f1_info.get_page_handle(pa(f1, 32)).is_some());
        assert!(f1_info.get_page_handle(pa(f1, 64)).is_some());
        assert!(f1_info.get_page_handle(pa(f1, 128)).is_none());

        let f4_info = virtual_infos.get(&f4).unwrap();
        assert!(f4_info.get_page_handle(pa(f4, 0)).is_none());
        assert!(f4_info.get_page_handle(pa(f2, 32)).is_none());
        assert!(f4_info.get_page_handle(pa(f4, 64)).is_some());

        let base_size = virtual_infos
            .values()
            .map(|info| info.effective_size())
            .sum::<usize>();
        let used_size = m3_info.file_size();
        println!("base size {base_size}");
        println!("used size {used_size}");
        assert!(base_size < used_size);
    }

    #[photonio::test]
    async fn map_files_reclaiming() {
        let root = TempDir::new("map_files_reclaiming").unwrap();
        let root = root.into_path();

        let rewriter = PageRewriter::default();
        let mut ctx = build_reclaim_ctx(&root, rewriter.clone()).await;

        let (f1, f2, f3, f4) = (1, 2, 3, 4);
        let (m1, m2, m3) = (1, 2, 3);
        ctx.next_map_file_id = m3;
        let mut pages = HashMap::new();
        pages.insert(f1, vec![(1, pa(f1, 16)), (2, pa(f1, 32)), (3, pa(f1, 64))]);
        pages.insert(f2, vec![(4, pa(f2, 16)), (5, pa(f2, 32)), (6, pa(f2, 64))]);
        let (virtual_infos, m1_info) = build_map_file(&ctx.page_files, m1, pages).await;
        let mut page_files = virtual_infos;

        let mut pages = HashMap::new();
        pages.insert(f3, vec![(7, pa(f3, 16)), (8, pa(f3, 32)), (9, pa(f3, 64))]);
        pages.insert(f4, vec![(1, pa(f4, 16)), (2, pa(f4, 32)), (3, pa(f4, 64))]);
        let (virtual_infos, m2_info) = build_map_file(&ctx.page_files, m2, pages).await;
        page_files.extend(virtual_infos.into_iter());

        let mut map_files = HashMap::new();
        map_files.insert(m1, m1_info);
        map_files.insert(m2, m2_info);
        let victims = HashSet::from_iter(vec![m1, m2].into_iter());

        let delta = DeltaVersion {
            reason: VersionUpdateReason::Flush,
            page_files,
            map_files,
            ..Default::default()
        };
        // No concurrent operations.
        unsafe { ctx.version_owner.install(delta) };
        let version = ctx.version_owner.current();
        let mut progress = ReclaimProgress::new(&ctx.options, &version, &HashSet::default());
        ctx.reclaim_map_files(&mut progress, &version, victims)
            .await;

        let version = ctx.version_owner.current();
        let page_files = version.page_files();
        assert!(page_files.contains_key(&f1));
        assert!(page_files.contains_key(&f2));
        assert!(page_files.contains_key(&f3));
        assert!(page_files.contains_key(&f4));

        let f1_info = page_files.get(&f1).unwrap();
        assert!(f1_info.get_page_handle(pa(f1, 32)).is_some());
        assert!(f1_info.get_page_handle(pa(f1, 64)).is_some());
        assert!(f1_info.get_page_handle(pa(f1, 128)).is_none());

        let f4_info = page_files.get(&f4).unwrap();
        assert!(f4_info.get_page_handle(pa(f4, 0)).is_none());
        assert!(f4_info.get_page_handle(pa(f2, 32)).is_none());
        assert!(f4_info.get_page_handle(pa(f4, 64)).is_some());

        let map_files = version.map_files();
        // The compacted map files are not contained in version.
        assert!(!map_files.contains_key(&m1));
        assert!(!map_files.contains_key(&m2));
        assert!(map_files.contains_key(&m3));
    }
}
