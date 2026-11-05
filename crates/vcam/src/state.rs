// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! State shared between the filter and its pin. Lives in an `Arc`; both
//! COM objects hold a clone. `filter_this` is set once after construction
//! (for `QueryPinInfo`) and cleared by the pin's `Drop`, breaking the
//! reference cycle whichever object is released first.

use std::sync::{Arc, Mutex};

use windows::Win32::Media::DirectShow::{FILTER_STATE, IBaseFilter, IFilterGraph, State_Stopped};

use crate::pin::PinState;

// Safety: every mutable field lives behind its own `Mutex`; COM interface
// pointers stored here are only touched while holding that mutex (or moved
// to the streaming thread as raw pointers whose owners outlive the thread,
// see `pin.rs`). DShow calls us from arbitrary COM worker threads ("Both"
// threading model), so the shared state must be Send+Sync.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Shared filter/pin state.
pub struct Shared {
    /// Filter state (Stopped/Paused/Running).
    pub filter_state: Mutex<FILTER_STATE>,
    /// Filter name given by the graph.
    pub name: Mutex<String>,
    /// Owning graph, while joined.
    pub graph: Mutex<Option<IFilterGraph>>,
    /// The filter's own `IBaseFilter` (cycle broken on pin Drop).
    pub filter_this: Mutex<Option<IBaseFilter>>,
    /// Pin state (see [`crate::pin::UdsCamPin`]).
    pub pin: Mutex<PinState>,
}

impl Shared {
    /// Empty shared state.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            filter_state: Mutex::new(State_Stopped),
            name: Mutex::new(String::new()),
            graph: Mutex::new(None),
            filter_this: Mutex::new(None),
            pin: Mutex::new(PinState::placeholder()),
        })
    }
}
