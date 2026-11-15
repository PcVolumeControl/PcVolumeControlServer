//! A PulseAudio context driven synchronously on the worker thread.
//!
//! The context is created through `libpulse-sys` rather than the safe
//! `libpulse-binding` `Context`, because the binding converts every
//! `pa_sink_info` into a `SinkInfo` whose `state` field is a Rust enum: a
//! server that reports a transient sink state outside that enum (PulseAudio's
//! internal `INIT` and `UNLINKED`) makes the conversion undefined behaviour.
//! Here each callback reads only the fields it needs, through raw place
//! projections, so `state` (and the other enum-typed fields such as the sample
//! spec and channel map) are never read.
//!
//! Every operation is waited for, or cancelled, before the stack data its
//! callback points at goes out of scope (see [`Op`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::time::{Duration, Instant};

use libpulse_binding::error::PAErr;
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::time::MicroSeconds;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use libpulse_sys as sys;

use super::model::{AppProps, RawSink, RawSinkInput, RawState};

/// Client name shown by the sound server.
const CLIENT_NAME: &CStr = c"PcVolumeControl";

/// Why an operation did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpError {
    /// The server rejected the request; carries the context error code.
    Failed(i32),
    /// No answer before the deadline.
    Timeout,
    /// The connection went away while waiting.
    Cancelled,
}

impl OpError {
    pub fn is_no_entity(&self) -> bool {
        *self == OpError::Failed(sys::PA_ERR_NOENTITY as i32)
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::Failed(code) => write!(f, "{}", PAErr(*code)),
            OpError::Timeout => f.write_str("timed out waiting for the sound server"),
            OpError::Cancelled => f.write_str("connection to the sound server lost"),
        }
    }
}

impl std::error::Error for OpError {}

/// Runs one prepare/poll/dispatch round, waiting at most `timeout` (`None`
/// blocks until something happens).
pub(super) fn iterate(mainloop: &mut Mainloop, timeout: Option<Duration>) {
    let timeout = timeout.map(|t| {
        // The C API takes an `int` of microseconds.
        MicroSeconds(t.as_micros().min(i32::MAX as u128) as u64)
    });
    let result = mainloop
        .prepare(timeout)
        .and_then(|()| mainloop.poll())
        .and_then(|_| mainloop.dispatch());
    if let Err(e) = result {
        tracing::warn!("PulseAudio main loop iteration failed: {e}");
        // Avoid spinning if the loop keeps failing.
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Iterates until `done` returns true or `deadline` passes; returns whether
/// `done` was reached.
fn wait_until(mainloop: &mut Mainloop, deadline: Instant, mut done: impl FnMut() -> bool) -> bool {
    loop {
        if done() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        iterate(mainloop, Some(deadline - now));
    }
}

/// An owned operation reference. Dropping it cancels the operation if it is
/// still running, so its callback can no longer run, then releases it.
struct Op(*mut sys::pa_operation);

impl Op {
    /// Wraps a freshly started operation; null means the request could not be
    /// sent.
    fn new(ptr: *mut sys::pa_operation, ctx: *mut sys::pa_context) -> Result<Self, OpError> {
        if ptr.is_null() {
            Err(OpError::Failed(errno(ctx)))
        } else {
            Ok(Self(ptr))
        }
    }

    fn state(&self) -> sys::pa_operation_state_t {
        // SAFETY: self.0 is a live operation we hold a reference to.
        unsafe { sys::pa_operation_get_state(self.0) }
    }

    fn wait(&self, mainloop: &mut Mainloop, deadline: Instant) -> Result<(), OpError> {
        let finished = wait_until(mainloop, deadline, || {
            self.state() != sys::pa_operation_state_t::Running
        });
        match self.state() {
            sys::pa_operation_state_t::Done => Ok(()),
            _ if !finished => Err(OpError::Timeout),
            _ => Err(OpError::Cancelled),
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        // SAFETY: self.0 is a live operation we hold a reference to; after
        // cancelling, libpulse no longer invokes its callback.
        unsafe {
            if self.state() == sys::pa_operation_state_t::Running {
                sys::pa_operation_cancel(self.0);
            }
            sys::pa_operation_unref(self.0);
        }
    }
}

fn errno(ctx: *mut sys::pa_context) -> i32 {
    // SAFETY: ctx is a live context.
    unsafe { sys::pa_context_errno(ctx) }
}

/// Copies a C string that is only valid during a callback.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string.
unsafe fn owned_cstr(p: *const c_char) -> Option<CString> {
    // SAFETY: guaranteed by the caller.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_owned())
}

/// # Safety
/// `p` must be null or point to a NUL-terminated string.
unsafe fn lossy(p: *const c_char) -> Option<String> {
    // SAFETY: guaranteed by the caller.
    unsafe { owned_cstr(p) }.map(|s| s.to_string_lossy().into_owned())
}

/// # Safety
/// `proplist` must be null or a live property list.
unsafe fn prop(proplist: *const sys::pa_proplist, key: &CStr) -> Option<String> {
    if proplist.is_null() {
        return None;
    }
    // SAFETY: proplist is live; the returned string (or null) stays valid
    // while the proplist is unchanged, and it is copied immediately.
    unsafe { lossy(sys::pa_proplist_gets(proplist, key.as_ptr())) }
}

fn channel_volumes(cv: &sys::pa_cvolume) -> Vec<u32> {
    let n = usize::from(cv.channels).min(cv.values.len());
    cv.values[..n].to_vec()
}

/// A volume with every channel at `value`, or `None` for a channel count
/// libpulse rejects. The count is checked up front because `pa_cvolume_set`
/// aborts the process on an invalid one.
pub(super) fn uniform_volume(channels: usize, value: u32) -> Option<ChannelVolumes> {
    let channels = u8::try_from(channels)
        .ok()
        .filter(|c| (1..=sys::PA_CHANNELS_MAX).contains(c))?;
    let mut cv = ChannelVolumes::default();
    cv.set(channels, Volume(value));
    cv.is_valid().then_some(cv)
}

/// Collects the items of a list query.
struct ListAcc<T> {
    items: RefCell<Vec<T>>,
    failed: Cell<bool>,
}

impl<T> ListAcc<T> {
    fn new() -> Self {
        Self {
            items: RefCell::new(Vec::new()),
            failed: Cell::new(false),
        }
    }

    fn userdata(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    fn finish(self) -> Result<Vec<T>, OpError> {
        if self.failed.get() {
            Err(OpError::Failed(sys::PA_ERR_UNKNOWN as i32))
        } else {
            Ok(self.items.into_inner())
        }
    }
}

/// Shared shape of the list callbacks: `read` turns a non-null item into a
/// record, or `None` to skip it.
///
/// # Safety
/// `userdata` must point to a live `ListAcc<T>`.
unsafe fn list_item<I, T>(
    item: *const I,
    eol: i32,
    userdata: *mut c_void,
    read: impl FnOnce(*const I) -> Option<T>,
) {
    // SAFETY: guaranteed by the caller.
    let Some(acc) = (unsafe { userdata.cast::<ListAcc<T>>().as_ref() }) else {
        return;
    };
    if eol < 0 {
        acc.failed.set(true);
    } else if eol == 0
        && !item.is_null()
        && let Some(record) = read(item)
        && let Ok(mut items) = acc.items.try_borrow_mut()
    {
        items.push(record);
    }
}

extern "C" fn sink_cb(
    _: *mut sys::pa_context,
    info: *const sys::pa_sink_info,
    eol: i32,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the ListAcc<RawSink> passed by `Connection::query`;
    // `info` is valid for the duration of the callback and only plain
    // integer and pointer fields are read from it.
    unsafe {
        list_item(info, eol, userdata, |p| {
            Some(RawSink {
                index: (&raw const (*p).index).read(),
                name: owned_cstr((&raw const (*p).name).read())?,
                description: lossy((&raw const (*p).description).read()),
                volume: channel_volumes(&(&raw const (*p).volume).read()),
                muted: (&raw const (*p).mute).read() != 0,
            })
        });
    }
}

extern "C" fn sink_input_cb(
    _: *mut sys::pa_context,
    info: *const sys::pa_sink_input_info,
    eol: i32,
    userdata: *mut c_void,
) {
    // SAFETY: as for `sink_cb`, with a ListAcc<RawSinkInput>.
    unsafe {
        list_item(info, eol, userdata, |p| {
            let props = (&raw const (*p).proplist).read();
            let client = (&raw const (*p).client).read();
            Some(RawSinkInput {
                index: (&raw const (*p).index).read(),
                sink: (&raw const (*p).sink).read(),
                client: (client != sys::PA_INVALID_INDEX).then_some(client),
                volume: channel_volumes(&(&raw const (*p).volume).read()),
                has_volume: (&raw const (*p).has_volume).read() != 0,
                volume_writable: (&raw const (*p).volume_writable).read() != 0,
                muted: (&raw const (*p).mute).read() != 0,
                app: AppProps::read(|key| prop(props, key)),
                media_name: prop(props, c"media.name"),
                media_title: prop(props, c"media.title"),
                media_role: prop(props, c"media.role"),
            })
        });
    }
}

extern "C" fn client_cb(
    _: *mut sys::pa_context,
    info: *const sys::pa_client_info,
    eol: i32,
    userdata: *mut c_void,
) {
    // SAFETY: as for `sink_cb`, with a ListAcc<(u32, AppProps)>.
    unsafe {
        list_item(info, eol, userdata, |p| {
            let props = (&raw const (*p).proplist).read();
            Some((
                (&raw const (*p).index).read(),
                AppProps::read(|key| prop(props, key)),
            ))
        });
    }
}

/// Result slot of a server-info query.
#[derive(Default)]
struct ServerAcc {
    default_sink: RefCell<Option<CString>>,
    failed: Cell<bool>,
}

extern "C" fn server_cb(
    _: *mut sys::pa_context,
    info: *const sys::pa_server_info,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the ServerAcc passed by `Connection::query`; only
    // the default sink name pointer is read from `info`.
    unsafe {
        let Some(acc) = userdata.cast::<ServerAcc>().as_ref() else {
            return;
        };
        if info.is_null() {
            acc.failed.set(true);
            return;
        }
        let name = owned_cstr((&raw const (*info).default_sink_name).read());
        if let Ok(mut slot) = acc.default_sink.try_borrow_mut() {
            *slot = name;
        }
    }
}

extern "C" fn success_cb(_: *mut sys::pa_context, success: i32, userdata: *mut c_void) {
    // SAFETY: userdata is the Cell passed by `Connection::run`.
    if let Some(slot) = unsafe { userdata.cast::<Cell<Option<bool>>>().as_ref() } {
        slot.set(Some(success != 0));
    }
}

extern "C" fn subscribe_cb(
    _: *mut sys::pa_context,
    event: sys::pa_subscription_event_type_t,
    _: u32,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the connection's boxed dirty flags, which outlive
    // the callback registration.
    if let Some(dirty) = unsafe { userdata.cast::<Dirty>().as_ref() } {
        dirty.state.set(true);
        if event & sys::PA_SUBSCRIPTION_EVENT_FACILITY_MASK == sys::PA_SUBSCRIPTION_EVENT_CLIENT {
            dirty.clients.set(true);
        }
    }
}

/// What the server reported as changed since it was last read.
struct Dirty {
    state: Cell<bool>,
    /// A client appeared, went away or changed its properties.
    clients: Cell<bool>,
}

/// A connected context subscribed to sink, sink-input, client and server
/// changes.
///
/// It must be dropped before the main loop it was created on.
pub(super) struct Connection {
    ctx: *mut sys::pa_context,
    /// Set by the subscription callback; boxed so its address is stable.
    dirty: Box<Dirty>,
}

impl Connection {
    /// Connects to the default server and subscribes to changes.
    pub fn open(mainloop: &mut Mainloop, timeout: Duration) -> Result<Self, OpError> {
        let deadline = Instant::now() + timeout;
        let api: &sys::pa_mainloop_api = mainloop.get_api().as_ref();
        // SAFETY: the API vtable belongs to `mainloop`, which outlives the
        // context (see the type docs).
        let ctx = unsafe { sys::pa_context_new(api, CLIENT_NAME.as_ptr()) };
        if ctx.is_null() {
            return Err(OpError::Failed(sys::PA_ERR_INTERNAL as i32));
        }
        // From here on, Drop releases the context.
        let conn = Self {
            ctx,
            dirty: Box::new(Dirty {
                state: Cell::new(true),
                clients: Cell::new(true),
            }),
        };
        // SAFETY: ctx is live; the flags outlive the registration, which Drop
        // removes before freeing the box.
        unsafe {
            sys::pa_context_set_subscribe_callback(
                ctx,
                Some(subscribe_cb),
                ptr::from_ref(&*conn.dirty).cast_mut().cast(),
            );
        }
        // SAFETY: ctx is live; a null server means the default one.
        let rc = unsafe {
            sys::pa_context_connect(ctx, ptr::null(), sys::PA_CONTEXT_NOAUTOSPAWN, ptr::null())
        };
        if rc < 0 {
            return Err(OpError::Failed(errno(ctx)));
        }
        wait_until(mainloop, deadline, || {
            !matches!(
                conn.state(),
                sys::pa_context_state_t::Unconnected
                    | sys::pa_context_state_t::Connecting
                    | sys::pa_context_state_t::Authorizing
                    | sys::pa_context_state_t::SettingName
            )
        });
        match conn.state() {
            sys::pa_context_state_t::Ready => {}
            sys::pa_context_state_t::Failed | sys::pa_context_state_t::Terminated => {
                return Err(OpError::Failed(errno(ctx)));
            }
            _ => return Err(OpError::Timeout),
        }
        let mask = sys::PA_SUBSCRIPTION_MASK_SINK
            | sys::PA_SUBSCRIPTION_MASK_SINK_INPUT
            | sys::PA_SUBSCRIPTION_MASK_CLIENT
            | sys::PA_SUBSCRIPTION_MASK_SERVER;
        // SAFETY: ctx is live; `run` keeps the userdata alive until the
        // operation finishes or is cancelled.
        conn.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_subscribe(ctx, mask, cb, ud)
        })?;
        Ok(conn)
    }

    fn state(&self) -> sys::pa_context_state_t {
        // SAFETY: ctx is live.
        unsafe { sys::pa_context_get_state(self.ctx) }
    }

    /// Whether the connection has failed or been closed.
    pub fn is_dead(&self) -> bool {
        matches!(
            self.state(),
            sys::pa_context_state_t::Failed | sys::pa_context_state_t::Terminated
        )
    }

    /// Returns whether a change was reported since the last call, and clears
    /// the flag.
    pub fn is_dirty(&self) -> bool {
        self.dirty.state.get()
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.state.replace(false)
    }

    /// Starts an operation with a success callback via `start(callback,
    /// userdata)` and waits for its result.
    fn run(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        start: impl FnOnce(sys::pa_context_success_cb_t, *mut c_void) -> *mut sys::pa_operation,
    ) -> Result<(), OpError> {
        let slot = Cell::new(None::<bool>);
        let op = Op::new(
            start(Some(success_cb), ptr::from_ref(&slot).cast_mut().cast()),
            self.ctx,
        )?;
        op.wait(mainloop, deadline)?;
        drop(op);
        match slot.get() {
            Some(true) => Ok(()),
            Some(false) => Err(OpError::Failed(errno(self.ctx))),
            None => Err(OpError::Cancelled),
        }
    }

    /// Reads the default sink, the sinks and the sink-inputs. The clients
    /// are re-read only when one changed or a stream refers to one not in
    /// `known_clients`; they only provide names and hints, so failing to read
    /// them keeps `known_clients` rather than failing the read.
    pub fn query(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        known_clients: &HashMap<u32, AppProps>,
    ) -> Result<RawState, OpError> {
        let clients_dirty = self.dirty.clients.replace(false);
        let server = ServerAcc::default();
        let sinks = ListAcc::<RawSink>::new();
        let inputs = ListAcc::<RawSinkInput>::new();
        // The operations are declared after the accumulators so they are
        // dropped (cancelled) first on every return path.
        // SAFETY: ctx is live and each accumulator outlives its operation.
        let server_op = Op::new(
            unsafe {
                sys::pa_context_get_server_info(
                    self.ctx,
                    Some(server_cb),
                    ptr::from_ref(&server).cast_mut().cast(),
                )
            },
            self.ctx,
        )?;
        let sinks_op = Op::new(
            // SAFETY: as for the server info operation.
            unsafe {
                sys::pa_context_get_sink_info_list(self.ctx, Some(sink_cb), sinks.userdata())
            },
            self.ctx,
        )?;
        let inputs_op = Op::new(
            // SAFETY: as for the server info operation.
            unsafe {
                sys::pa_context_get_sink_input_info_list(
                    self.ctx,
                    Some(sink_input_cb),
                    inputs.userdata(),
                )
            },
            self.ctx,
        )?;
        server_op.wait(mainloop, deadline)?;
        sinks_op.wait(mainloop, deadline)?;
        inputs_op.wait(mainloop, deadline)?;
        drop((server_op, sinks_op, inputs_op));
        if server.failed.get() {
            return Err(OpError::Failed(sys::PA_ERR_UNKNOWN as i32));
        }
        let inputs = inputs.finish()?;
        let unknown_client = inputs
            .iter()
            .filter_map(|i| i.client)
            .any(|c| !known_clients.contains_key(&c));
        let clients = if clients_dirty || unknown_client {
            self.query_clients(mainloop, deadline).unwrap_or_else(|e| {
                tracing::debug!("reading PulseAudio clients failed: {e}");
                self.dirty.clients.set(true);
                known_clients.clone()
            })
        } else {
            known_clients.clone()
        };
        Ok(RawState {
            default_sink: server.default_sink.into_inner(),
            sinks: sinks.finish()?,
            inputs,
            clients,
        })
    }

    fn query_clients(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
    ) -> Result<HashMap<u32, AppProps>, OpError> {
        let clients = ListAcc::<(u32, AppProps)>::new();
        let op = Op::new(
            // SAFETY: ctx is live and the accumulator outlives the operation.
            unsafe {
                sys::pa_context_get_client_info_list(self.ctx, Some(client_cb), clients.userdata())
            },
            self.ctx,
        )?;
        op.wait(mainloop, deadline)?;
        drop(op);
        Ok(clients.finish()?.into_iter().collect())
    }

    pub fn set_default_sink(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        name: &CStr,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live; libpulse copies the name before returning.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_set_default_sink(self.ctx, name.as_ptr(), cb, ud)
        })
    }

    pub fn set_sink_volume(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        index: u32,
        volume: &ChannelVolumes,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live; libpulse copies the volume before returning.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_set_sink_volume_by_index(self.ctx, index, volume.as_ref(), cb, ud)
        })
    }

    pub fn set_sink_mute(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        index: u32,
        muted: bool,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_set_sink_mute_by_index(self.ctx, index, i32::from(muted), cb, ud)
        })
    }

    pub fn set_sink_input_volume(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        index: u32,
        volume: &ChannelVolumes,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live; libpulse copies the volume before returning.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_set_sink_input_volume(self.ctx, index, volume.as_ref(), cb, ud)
        })
    }

    pub fn set_sink_input_mute(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        index: u32,
        muted: bool,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_set_sink_input_mute(self.ctx, index, i32::from(muted), cb, ud)
        })
    }

    pub fn move_sink_input(
        &self,
        mainloop: &mut Mainloop,
        deadline: Instant,
        index: u32,
        sink: u32,
    ) -> Result<(), OpError> {
        // SAFETY: ctx is live.
        self.run(mainloop, deadline, |cb, ud| unsafe {
            sys::pa_context_move_sink_input_by_index(self.ctx, index, sink, cb, ud)
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: ctx is live and owned by us. Clearing the subscription
        // callback first means `dirty` is no longer referenced once it is
        // freed after this.
        unsafe {
            sys::pa_context_set_subscribe_callback(self.ctx, None, ptr::null_mut());
            sys::pa_context_disconnect(self.ctx);
            sys::pa_context_unref(self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_volume_validates_channels() {
        assert!(uniform_volume(0, 1).is_none());
        assert!(uniform_volume(33, 1).is_none());
        assert!(uniform_volume(300, 1).is_none());
        let cv = uniform_volume(2, 1234).unwrap();
        assert_eq!(channel_volumes(cv.as_ref()), vec![1234, 1234]);
        assert_eq!(uniform_volume(32, 7).unwrap().len(), 32);
    }

    #[test]
    fn channel_volumes_bounds_channel_count() {
        let cv = sys::pa_cvolume {
            channels: 200,
            values: [5; sys::PA_CHANNELS_MAX as usize],
        };
        assert_eq!(channel_volumes(&cv).len(), 32);
    }

    #[test]
    fn op_error_messages() {
        assert!(OpError::Failed(sys::PA_ERR_NOENTITY as i32).is_no_entity());
        assert!(!OpError::Timeout.is_no_entity());
        assert_eq!(
            OpError::Failed(sys::PA_ERR_NOENTITY as i32).to_string(),
            "No such entity"
        );
    }
}
