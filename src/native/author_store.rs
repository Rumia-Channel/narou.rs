use std::sync::Arc;
use crate::application::authors::AuthorStore;
use crate::author::{self, AuthorRecord};
use crate::db::inventory::Inventory;
use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

#[derive(Clone)]
pub struct NativeAuthorStore { inventory: Arc<Inventory> }

impl NativeAuthorStore {
    pub fn new(inventory: Arc<Inventory>) -> Self { Self { inventory } }
    pub fn for_current_root() -> Result<Self> {
        Ok(Self::new(Arc::new(Inventory::with_default_root()?)))
    }
}

impl AuthorStore for NativeAuthorStore {
    fn load(&self) -> PlatformFuture<'_, Result<Vec<AuthorRecord>>> {
        let inventory = self.inventory.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || author::load_authors(&inventory)).await
                .map_err(|error| NarouError::Platform(format!("author load task failed: {error}")))?
        })
    }
    fn add<'a>(&'a self, author: &'a AuthorRecord) -> PlatformFuture<'a, Result<bool>> {
        let inventory = self.inventory.clone();
        let author = author.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || author::add_author_in(&inventory, &author)).await
                .map_err(|error| NarouError::Platform(format!("author add task failed: {error}")))?
        })
    }
    fn remove<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<bool>> {
        let inventory = self.inventory.clone();
        let url = url.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || author::remove_author_in(&inventory, &url)).await
                .map_err(|error| NarouError::Platform(format!("author remove task failed: {error}")))?
        })
    }
}
