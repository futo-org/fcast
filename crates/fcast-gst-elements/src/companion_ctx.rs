//! The host contract `fcompsrc` resolves its `fcomp://` URIs through.
//!
//! `fcompsrc` is handed a [`CompanionContext`] on its `context` property and
//! asks it for the provider named in the URI. A provider is whatever session
//! registered itself as able to serve companion resources. Keeping the
//! contract here rather than in the receiver lets the element and its tests
//! build without the receiver.

use std::{collections::HashMap, sync::Arc};

use fcast_protocol::{companion, v4};
use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::debug;

use v4::flat::CompanionResourceInfoResponse;

self_cell::self_cell!(
    pub struct ResourceInfoResponseCell {
        owner: Vec<u8>,
        #[covariant]
        dependent: CompanionResourceInfoResponse,
    }

    impl {Debug, PartialEq}
);

pub type CompanionMsgSender = UnboundedSender<CompanionMessage>;
pub type CompanionMsgReceiver = UnboundedReceiver<CompanionMessage>;

pub enum FeedbackSender<T> {
    Channel(tokio::sync::mpsc::UnboundedSender<T>),
}

impl<T> FeedbackSender<T> {
    pub fn send(&self, obj: T) {
        match self {
            FeedbackSender::Channel(sender) => {
                let _ = sender.send(obj);
            }
        }
    }
}

pub enum CompanionMessage {
    GetResourceInfo {
        id: companion::ResourceId,
        feedback: FeedbackSender<ResourceInfoResponseCell>,
    },
    GetResource {
        id: companion::ResourceId,
        read_head: Option<v4::flat::ResourceReadHead>,
        feedback: FeedbackSender<companion::ResourceResponse>,
    },
}

#[derive(Clone)]
pub struct CompanionProviderHandle {
    tx: CompanionMsgSender,
}

impl CompanionProviderHandle {
    pub fn get_resource_info(
        &self,
        resource_id: companion::ResourceId,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<ResourceInfoResponseCell>, CompanionGone> {
        let (feedback, rx) = tokio::sync::mpsc::unbounded_channel();
        self.tx
            .send(CompanionMessage::GetResourceInfo {
                id: resource_id,
                feedback: FeedbackSender::Channel(feedback),
            })
            .map_err(|_| CompanionGone)?;
        Ok(rx)
    }

    pub fn get_resource(
        &self,
        resource_id: companion::ResourceId,
        read_head: Option<v4::flat::ResourceReadHead>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<companion::ResourceResponse>, CompanionGone>
    {
        let (feedback, rx) = tokio::sync::mpsc::unbounded_channel();
        self.tx
            .send(CompanionMessage::GetResource {
                id: resource_id,
                read_head,
                feedback: FeedbackSender::Channel(feedback),
            })
            .map_err(|_| CompanionGone)?;
        Ok(rx)
    }
}

/// The provider hung up between the lookup and the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompanionGone;

impl std::fmt::Display for CompanionGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the companion provider is gone")
    }
}

impl std::error::Error for CompanionGone {}

#[derive(Default)]
struct InnerCompanionContext {
    providers: HashMap<companion::ProviderId, CompanionProviderHandle>,
}

impl InnerCompanionContext {
    fn register_provider(&mut self, tx: CompanionMsgSender) -> companion::ProviderId {
        let mut id = 0;
        while self.providers.contains_key(&id) {
            id += 1;
        }

        let handle = CompanionProviderHandle { tx };
        self.providers.insert(id, handle);

        id
    }

    pub fn unregister_provider(&mut self, id: companion::ProviderId) {
        debug!(id, "Unregistering provider");
        self.providers.remove(&id);
    }
}

#[derive(Clone)]
pub struct CompanionContext(Arc<Mutex<InnerCompanionContext>>);

impl Default for CompanionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CompanionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CompanionContext").finish()
    }
}

impl CompanionContext {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(InnerCompanionContext::default())))
    }

    pub fn register_provider(&self, tx: CompanionMsgSender) -> companion::ProviderId {
        self.0.lock().register_provider(tx)
    }

    pub fn unregister_provider(&self, id: companion::ProviderId) {
        self.0.lock().unregister_provider(id)
    }

    pub fn get_provider(&self, id: companion::ProviderId) -> Option<CompanionProviderHandle> {
        self.0.lock().providers.get(&id).cloned()
    }
}
