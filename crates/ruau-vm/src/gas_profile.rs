use ruau_vm_api::{RawGc, marker};

use crate::{debug, heap::Heap, object::Proto};

/// Deterministic gas attribution for one VM invocation.
///
/// Entries are reported in first-executed site order. The total gas across all
/// entries equals the VM's invocation gas total for profiled invocations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GasProfile {
    entries: Vec<GasProfileEntry>,
}

impl GasProfile {
    pub(crate) fn new(entries: Vec<GasProfileEntry>) -> Self {
        Self { entries }
    }

    /// Per-source gas buckets for the invocation.
    #[must_use]
    pub fn entries(&self) -> &[GasProfileEntry] {
        &self.entries
    }

    /// Sum of all attributed gas units.
    #[must_use]
    pub fn total_gas(&self) -> u64 {
        self.entries.iter().map(|entry| entry.gas).sum()
    }

    /// Whether the invocation spent no attributable gas.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Gas spent at one chunk/line bucket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GasProfileEntry {
    /// Chunk name in the same display form as runtime errors and `debug.info`.
    /// `None` is reserved for charges that have no Lua source site.
    pub chunk_name: Option<String>,
    /// Function debug name, when the prototype carries one.
    pub function_name: Option<String>,
    /// 1-based Luau source line. `None` means the prototype carried no line info.
    pub line: Option<u32>,
    /// Gas units charged to this site.
    pub gas: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasProfileSite {
    proto: RawGc<Proto>,
    source: Option<RawGc<marker::Str>>,
    line: Option<u32>,
}

impl GasProfileSite {
    #[must_use]
    pub(crate) fn new(
        proto: RawGc<Proto>,
        source: Option<RawGc<marker::Str>>,
        line: Option<u32>,
    ) -> Self {
        Self {
            proto,
            source,
            line,
        }
    }
}

#[derive(Default)]
pub struct GasProfileRecorder {
    current: Option<GasProfileSite>,
    counters: Vec<GasProfileCounter>,
}

impl GasProfileRecorder {
    pub(crate) fn set_current_site(&mut self, site: GasProfileSite) {
        self.current = Some(site);
    }

    pub(crate) fn clear_current_site(&mut self) {
        self.current = None;
    }

    pub(crate) fn record(&mut self, units: u64) {
        if units == 0 {
            return;
        }
        let Some(site) = self.current else {
            return;
        };
        if let Some(counter) = self
            .counters
            .iter_mut()
            .find(|counter| counter.site == site)
        {
            counter.gas = counter.gas.saturating_add(units);
            return;
        }
        self.counters.push(GasProfileCounter { site, gas: units });
    }

    pub(crate) fn finish(self, heap: &Heap) -> GasProfile {
        let entries = self
            .counters
            .into_iter()
            .filter(|counter| counter.gas > 0)
            .map(|counter| GasProfileEntry {
                chunk_name: counter.site.source.and_then(|source| {
                    heap.string(source).map(|string| {
                        String::from_utf8_lossy(&debug::chunk_id(string.bytes())).into_owned()
                    })
                }),
                function_name: heap
                    .proto(counter.site.proto)
                    .and_then(|proto| proto.debug_name)
                    .and_then(|name| heap.string(name))
                    .map(|name| String::from_utf8_lossy(name.bytes()).into_owned()),
                line: counter.site.line,
                gas: counter.gas,
            })
            .collect();
        GasProfile::new(entries)
    }
}

struct GasProfileCounter {
    site: GasProfileSite,
    gas: u64,
}
