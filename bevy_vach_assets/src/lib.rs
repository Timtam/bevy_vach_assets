use bevy::{
    asset::io::{
        AssetReader, AssetReaderError, AssetSource, AssetSourceId, ErasedAssetReader, PathStream,
        Reader, VecReader,
    },
    platform::collections::HashMap,
    prelude::{App, AssetApp, AssetPlugin, Plugin},
};
use futures_lite::Stream;
use std::{
    fs::File,
    io::{Cursor, Read, Seek},
    path::{Path, PathBuf},
    pin::Pin,
    task::Poll,
};
use vach::{
    archive::{Archive, ArchiveConfig},
    prelude::VerifyingKey,
    PUBLIC_KEY_LENGTH,
};

pub use vach;

pub const ASSETS_DIR: &str = "assets";

pub const ASSETS_ARCHIVE: &str = "assets.bva";
pub const ARCHIVE_DIR: &str = ".";
pub const ARCHIVE_MAGIC: &[u8; vach::MAGIC_LENGTH] = b"BVA42"; // BVA = Bevy Vach Archive

pub const SECRETS_DIR: &str = "secrets";
pub const SECRETS_PUBLIC_KEY: &str = "key.pub";
pub const SECRETS_PRIVATE_KEY: &str = "key.sec";
pub const SECRETS_KEY_PAIR: &str = "key.pair";

pub const ASSET_FILE_INDEX: &str = "📇";
pub const ASSET_FILE_INDEX_SEP: &str = "|BVA|";

#[derive(Default, Debug, Clone)]
pub struct BevyVachAssetsPlugin {
    // note: add properties if/when needed
    pub public_key_bytes: Option<&'static [u8; PUBLIC_KEY_LENGTH]>,
    pub static_archive: Option<&'static [u8]>,
}

impl Plugin for BevyVachAssetsPlugin {
    fn build(&self, app: &mut App) {
        if app.is_plugin_added::<AssetPlugin>() {
            bevy::log::error!("BevyVachAssetsPlugin must be added before AssetPlugin");
        }

        // needed to move the values into the closure
        let public_key_bytes = self.public_key_bytes;
        let static_archive = self.static_archive;

        let source = AssetSource::build().with_reader(move || {
            Box::new(BevyVachAssetReader::new(public_key_bytes, static_archive))
        });
        app.register_asset_source(AssetSourceId::Default, source);
    }

    fn finish(&self, _app: &mut App) {}
}

trait ReadExt: Read + Seek + Send + Sync + 'static {}

impl ReadExt for File {}
impl ReadExt for Cursor<Box<[u8]>> {}
impl ReadExt for Cursor<Vec<u8>> {}
impl ReadExt for Cursor<&'static [u8]> {}

type Readable = Box<dyn ReadExt>;

struct BevyVachAssetReader {
    archive: Archive<Readable>,
    lookup: HashMap<PathBuf, String>,
    fallback: Option<Box<dyn ErasedAssetReader>>,
}

impl std::fmt::Debug for BevyVachAssetReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundledAssetReader").finish_non_exhaustive()
    }
}

impl BevyVachAssetReader {
    /// Create an empty [`BundledAssetReader`].
    #[must_use]
    pub fn new(
        public_key_bytes: Option<&'static [u8; PUBLIC_KEY_LENGTH]>,
        static_archive: Option<&'static [u8]>,
    ) -> Self {
        // TODO: needs better setup handling! see pieces below

        let mut config = ArchiveConfig::default().magic(*ARCHIVE_MAGIC);
        // todo: currently it silently fails if the key is not valid
        config = public_key_bytes
            .and_then(|b| VerifyingKey::from_bytes(b).ok())
            .map_or(config, |k| config.key(k));

        // todo: find a reliable way to use fetch API instead of embedding the archive
        // note: tried to use web-sys and wrapping in a TaskPool, but always panicked on
        //       an option unwrap for results when awaiting the fetch; no idea what's up
        let target = if let Some(archive) = static_archive {
            let cursor = Cursor::new(archive);
            let boxed = Box::new(cursor);
            boxed
        } else if cfg!(target_arch = "wasm32") {
            bevy::log::error!("no static/embedded archive found, but required for wasm target");
            panic!("no static/embedded archive found, but required for wasm target")
        } else {
            let dir = std::env::current_dir().expect("could not get current directory");
            let archive_path = dir.join(ARCHIVE_DIR).join(ASSETS_ARCHIVE);
            let f = File::open(archive_path).expect("could not open the asset archive file");
            let boxed: Readable = Box::new(f);
            boxed
        };

        let mut archive = Archive::with_config(target, &config).expect("oops");

        let file_index = archive
            .fetch_mut(ASSET_FILE_INDEX)
            .expect("fetch index file");
        let files = String::from_utf8_lossy(&file_index.data);
        let files = files.split(ASSET_FILE_INDEX_SEP).collect::<Vec<_>>();

        let mut lookup = HashMap::new();
        for (id, path) in files.iter().enumerate() {
            lookup.insert(PathBuf::from(path), id.to_string());
        }

        Self {
            archive,
            lookup,
            fallback: None,
        }
    }

    #[allow(dead_code)]
    // #[must_use]
    pub fn new_with_fallback(
        public_key_bytes: Option<&'static [u8; vach::PUBLIC_KEY_LENGTH]>,
        static_archive: Option<&'static [u8]>,
        mut fallback: impl FnMut() -> Box<dyn ErasedAssetReader> + Send + Sync + 'static,
    ) -> Self {
        let mut reader = Self::new(public_key_bytes, static_archive);
        reader.fallback = Some(fallback());
        reader
    }

    /// Get the data from the asset matching the path provided.
    ///
    /// # Errors
    ///
    /// This will returns an error if the path is not known.
    fn load_path_sync<'a>(&'a self, path: &'a Path) -> Result<VecReader, AssetReaderError> {
        self.lookup
            .get(path)
            .and_then(|id| self.archive.fetch(id).ok())
            .map(|r| VecReader::new(r.data.to_vec()))
            .ok_or_else(|| AssetReaderError::NotFound(path.to_path_buf()))
    }

    fn has_file_sync(&self, path: &Path) -> bool {
        self.lookup.contains_key(path)
    }

    fn is_directory_sync(&self, path: &Path) -> bool {
        let as_folder = path.join("");
        self.lookup
            .keys()
            .any(|loaded_path| loaded_path.starts_with(&as_folder) && loaded_path != path)
    }

    fn read_directory_sync(&self, path: &Path) -> Result<DirReader, AssetReaderError> {
        if self.is_directory_sync(path) {
            let paths: Vec<_> = self
                .lookup
                .keys()
                .filter(|loaded_path| loaded_path.starts_with(path))
                .cloned()
                .collect();
            Ok(DirReader(paths))
        } else {
            Err(AssetReaderError::NotFound(path.to_path_buf()))
        }
    }
}

impl AssetReader for BevyVachAssetReader {
    async fn read<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        if self.has_file_sync(path) {
            self.load_path_sync(path).map(|reader| {
                let boxed: Box<dyn Reader> = Box::new(reader);
                boxed
            })
        } else if let Some(fallback) = self.fallback.as_ref() {
            fallback.read(path).await
        } else {
            Err(AssetReaderError::NotFound(path.to_path_buf()))
        }
    }

    async fn read_meta<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        let meta_path = get_meta_path(path);

        if self.has_file_sync(&meta_path) {
            self.load_path_sync(&meta_path).map(|reader| {
                let boxed: Box<dyn Reader> = Box::new(reader);
                boxed
            })
        } else if let Some(fallback) = self.fallback.as_ref() {
            fallback.read_meta(path).await as Result<Box<dyn Reader>, AssetReaderError>
        } else {
            Err(AssetReaderError::NotFound(meta_path))
        }
    }

    async fn read_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Box<PathStream>, AssetReaderError> {
        self.read_directory_sync(path).map(|read_dir| {
            let boxed: Box<PathStream> = Box::new(read_dir);
            boxed
        })
    }

    async fn is_directory<'a>(&'a self, path: &'a Path) -> Result<bool, AssetReaderError> {
        Ok(self.is_directory_sync(path))
    }
}

struct DirReader(Vec<PathBuf>);

impl Stream for DirReader {
    type Item = PathBuf;

    fn poll_next(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Poll::Ready(this.0.pop())
    }
}

#[inline]
fn get_meta_path(path: &Path) -> PathBuf {
    let mut meta_path = path.to_path_buf();
    let mut extension = path
        .extension()
        .expect("asset paths must have extensions")
        .to_os_string();
    extension.push(".meta");
    meta_path.set_extension(extension);
    meta_path
}
