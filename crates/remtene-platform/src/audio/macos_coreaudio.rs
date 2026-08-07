#![allow(unsafe_code)]

//! macOS CoreAudio microphone backend and bounded PCM16 WAV writer.
//!
//! Native calls and callback pointer ownership are confined to this module. The
//! render callback performs only one `AudioUnitRender`, bounded writes into a
//! preallocated sink, and atomic fault reporting. File I/O is owned by a
//! dedicated writer thread.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{BufWriter, Seek, SeekFrom, Write},
    mem,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI16, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use coreaudio_sys::{
    AURenderCallbackStruct, AudioBuffer, AudioBufferList, AudioComponentDescription,
    AudioComponentFindNext, AudioComponentInstanceDispose, AudioComponentInstanceNew,
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef,
    AudioConverterSetProperty, AudioDeviceID, AudioObjectGetPropertyData,
    AudioObjectPropertyAddress, AudioOutputUnitStart, AudioOutputUnitStop,
    AudioStreamBasicDescription, AudioTimeStamp, AudioUnit, AudioUnitGetProperty,
    AudioUnitInitialize, AudioUnitRender, AudioUnitRenderActionFlags, AudioUnitSetProperty,
    AudioUnitUninitialize, OSStatus, kAudioConverterPrimeMethod, kAudioConverterQuality_High,
    kAudioConverterSampleRateConverterComplexity,
    kAudioConverterSampleRateConverterComplexity_MinimumPhase,
    kAudioConverterSampleRateConverterQuality, kAudioDevicePermissionsError,
    kAudioDevicePropertyNominalSampleRate, kAudioFormatFlagIsPacked,
    kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
    kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
    kAudioObjectUnknown, kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioOutputUnitProperty_SetInputCallback, kAudioUnitErr_TooManyFramesToProcess,
    kAudioUnitErr_Unauthorized, kAudioUnitManufacturer_Apple,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_StreamFormat,
    kAudioUnitScope_Global, kAudioUnitScope_Input, kAudioUnitScope_Output,
    kAudioUnitSubType_HALOutput, kAudioUnitType_Output, kConverterPrimeMethod_None,
};

use super::{
    AudioArtifactWriter, AudioBackend, AudioBackendHandle, AudioFormat, AudioFrameSink,
    AudioWriterFactory, AudioWriterRequest, BackendStartOutcome, BackendStartRequest,
    CANONICAL_ASR_AUDIO_FORMAT, CaptureFaultReporter, FrameSinkError, PortError, SafeAudioCapture,
    WriterPipeline, WriterSummary, port_error,
};

const INPUT_BUS: u32 = 1;
const OUTPUT_BUS: u32 = 0;
const DEFAULT_MAX_FRAMES_PER_SLICE: u32 = 4_096;
const MAX_ACCEPTED_FRAMES_PER_SLICE: u32 = 65_536;
const RING_SECONDS: usize = 2;
const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(2);
const WAV_HEADER_BYTES: u64 = 44;

/// CoreAudio AUHAL backend for the current default macOS input device.
///
/// The caller must establish microphone permission before `start`. Permission
/// acquisition belongs to the platform permission adapter rather than the
/// real-time capture backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacOsCoreAudioBackend;

impl MacOsCoreAudioBackend {
    /// Returns the native sample rate of the current default input device and
    /// the V1 canonical mono PCM16 representation.
    pub fn default_input_format() -> Result<AudioFormat, PortError> {
        let device = default_input_device()?;
        input_format(device)
    }
}

impl AudioBackend for MacOsCoreAudioBackend {
    fn capture_format(&self, _configured_fallback: AudioFormat) -> Result<AudioFormat, PortError> {
        Self::default_input_format()
    }

    fn start(&self, request: BackendStartRequest) -> BackendStartOutcome {
        if let Err(error) = validate_pcm16_mono(request.format) {
            return BackendStartOutcome::FailedClean(error);
        }
        let device = match default_input_device() {
            Ok(device) => device,
            Err(error) => return BackendStartOutcome::FailedClean(error),
        };
        let current_format = match input_format(device) {
            Ok(format) => format,
            Err(error) => return BackendStartOutcome::FailedClean(error),
        };
        if current_format != request.format {
            return BackendStartOutcome::FailedClean(native_error(
                "audio.coreaudio_default_device_changed",
                "errors.audio.device_unavailable",
                true,
            ));
        }

        // SAFETY: every native call receives initialized values of the exact
        // CoreAudio ABI type. Ownership of `unit` is transferred to the handle
        // before capture can start.
        unsafe { start_audio_unit(device, request) }
    }
}

fn input_format(device: AudioDeviceID) -> Result<AudioFormat, PortError> {
    let sample_rate_hz = device_sample_rate(device)?;
    Ok(AudioFormat {
        sample_rate_hz,
        channels: 1,
        bits_per_sample: 16,
    })
}

/// Creates the complete macOS microphone adapter. The backend captures at the
/// current device rate while the writer always emits the canonical ASR format.
pub fn create_default_macos_audio_capture(
    artifact_root: impl Into<PathBuf>,
) -> Result<SafeAudioCapture, PortError> {
    SafeAudioCapture::initialize(
        artifact_root,
        CANONICAL_ASR_AUDIO_FORMAT,
        Arc::new(MacOsCoreAudioBackend),
        Arc::new(Pcm16WavWriterFactory),
    )
}

unsafe fn start_audio_unit(
    device: AudioDeviceID,
    request: BackendStartRequest,
) -> BackendStartOutcome {
    let component_description = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_HALOutput,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };

    // SAFETY: the description lives for the duration of the lookup call.
    let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &component_description) };
    if component.is_null() {
        return BackendStartOutcome::FailedClean(native_error(
            "audio.coreaudio_component_missing",
            "errors.audio.device_unavailable",
            true,
        ));
    }

    let mut unit: AudioUnit = ptr::null_mut();
    // SAFETY: `unit` is a valid out pointer and is checked before use.
    let instance_status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
    if instance_status != 0 {
        let error = status_error(
            instance_status,
            "audio.coreaudio_instance",
            "errors.audio.device_unavailable",
            true,
        );
        if unit.is_null() {
            return BackendStartOutcome::FailedClean(error);
        }
        return failed_start_with_owned_handle(error, MacOsCoreAudioHandle::unstarted(unit));
    }
    if unit.is_null() {
        return BackendStartOutcome::FailedClean(native_error(
            "audio.coreaudio_instance",
            "errors.audio.device_unavailable",
            true,
        ));
    }

    // Native ownership moves into the handle immediately after instance
    // creation. Every later failure either proves disposal or returns this
    // handle as `FailedOwned` for a retry by `SafeAudioCapture`.
    let mut handle = MacOsCoreAudioHandle::unstarted(unit);
    let configured = unsafe { configure_audio_unit(unit, device, request.format) };
    let max_frames = match configured {
        Ok(max_frames) => max_frames,
        Err(error) => return failed_start_with_owned_handle(error, handle),
    };

    let mut callback_state = Box::new(CallbackState {
        unit,
        sink: request.sink,
        faults: request.faults.clone(),
        render_buffer: vec![0_i16; max_frames as usize].into_boxed_slice(),
    });
    let callback = AURenderCallbackStruct {
        inputProc: Some(render_callback),
        inputProcRefCon: (&mut *callback_state as *mut CallbackState).cast::<c_void>(),
    };
    handle.callback_state = Some(callback_state);
    // SAFETY: CoreAudio copies the callback record; its refcon points into a
    // stable Box retained until the audio unit has been disposed.
    if let Err(error) = check_status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_SetInputCallback,
                kAudioUnitScope_Global,
                OUTPUT_BUS,
                (&callback as *const AURenderCallbackStruct).cast::<c_void>(),
                size_u32::<AURenderCallbackStruct>(),
            )
        },
        "audio.coreaudio_callback",
        true,
    ) {
        return failed_start_with_owned_handle(error, handle);
    }

    // SAFETY: all required AUHAL properties and callback storage are valid.
    if let Err(error) = check_status(
        unsafe { AudioUnitInitialize(unit) },
        "audio.coreaudio_initialize",
        true,
    ) {
        return failed_start_with_owned_handle(error, handle);
    }
    handle.initialized = true;

    // SAFETY: initialized AUHAL instance with callback ownership held above.
    let start_status = unsafe { AudioOutputUnitStart(unit) };
    if start_status == 0 {
        handle.started = true;
        return BackendStartOutcome::Started(Box::new(handle));
    }

    let start_error = status_error(
        start_status,
        "audio.coreaudio_start",
        "errors.audio.device_unavailable",
        true,
    );
    failed_start_with_owned_handle(start_error, handle)
}

fn failed_start_with_owned_handle(
    error: PortError,
    mut handle: MacOsCoreAudioHandle,
) -> BackendStartOutcome {
    debug_assert!(!handle.started);
    match handle.release_unstarted() {
        Ok(()) => BackendStartOutcome::FailedClean(error),
        Err(_) => BackendStartOutcome::FailedOwned {
            error,
            handle: Box::new(handle),
        },
    }
}

unsafe fn configure_audio_unit(
    unit: AudioUnit,
    device: AudioDeviceID,
    format: AudioFormat,
) -> Result<u32, PortError> {
    let enable_input: u32 = 1;
    // SAFETY: all property data pointers refer to initialized values with the
    // sizes provided to CoreAudio.
    check_status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Input,
                INPUT_BUS,
                (&enable_input as *const u32).cast::<c_void>(),
                size_u32::<u32>(),
            )
        },
        "audio.coreaudio_enable_input",
        true,
    )?;

    let disable_output: u32 = 0;
    // SAFETY: property arguments match the AUHAL output-enable ABI.
    check_status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Output,
                OUTPUT_BUS,
                (&disable_output as *const u32).cast::<c_void>(),
                size_u32::<u32>(),
            )
        },
        "audio.coreaudio_disable_output",
        true,
    )?;

    // SAFETY: `device` came from the CoreAudio system object.
    check_status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                OUTPUT_BUS,
                (&device as *const AudioDeviceID).cast::<c_void>(),
                size_u32::<AudioDeviceID>(),
            )
        },
        "audio.coreaudio_select_device",
        true,
    )?;

    let bytes_per_frame = u32::from(format.bits_per_sample / 8);
    let stream_format = AudioStreamBasicDescription {
        mSampleRate: f64::from(format.sample_rate_hz),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: u32::from(format.channels),
        mBitsPerChannel: u32::from(format.bits_per_sample),
        mReserved: 0,
    };
    // SAFETY: the ASBD exactly describes the mono PCM16 buffer supplied by the
    // render callback.
    check_status(
        unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                INPUT_BUS,
                (&stream_format as *const AudioStreamBasicDescription).cast::<c_void>(),
                size_u32::<AudioStreamBasicDescription>(),
            )
        },
        "audio.coreaudio_stream_format",
        true,
    )?;

    let requested_max = DEFAULT_MAX_FRAMES_PER_SLICE;
    // Some devices expose this property as read-only. Setting it is a bounded
    // preference; querying the effective value below is authoritative.
    // SAFETY: property arguments match the UInt32 ABI.
    let _ = unsafe {
        AudioUnitSetProperty(
            unit,
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            OUTPUT_BUS,
            (&requested_max as *const u32).cast::<c_void>(),
            size_u32::<u32>(),
        )
    };

    let mut effective_max = 0_u32;
    let mut size = size_u32::<u32>();
    // SAFETY: both out pointers remain valid for the property query.
    check_status(
        unsafe {
            AudioUnitGetProperty(
                unit,
                kAudioUnitProperty_MaximumFramesPerSlice,
                kAudioUnitScope_Global,
                OUTPUT_BUS,
                (&mut effective_max as *mut u32).cast::<c_void>(),
                &mut size,
            )
        },
        "audio.coreaudio_max_frames",
        true,
    )?;
    if effective_max == 0 || effective_max > MAX_ACCEPTED_FRAMES_PER_SLICE {
        return Err(native_error(
            "audio.coreaudio_invalid_buffer_size",
            "errors.audio.device_unavailable",
            false,
        ));
    }
    Ok(effective_max)
}

struct CallbackState {
    unit: AudioUnit,
    sink: Arc<dyn AudioFrameSink>,
    faults: CaptureFaultReporter,
    render_buffer: Box<[i16]>,
}

// SAFETY: CoreAudio owns callback scheduling while the handle owns the stable
// Box. The owner never reads or mutates callback data until the native unit is
// stopped, uninitialized, and disposed.
unsafe impl Send for CallbackState {}

unsafe extern "C" fn render_callback(
    in_ref_con: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_action_flags.is_null() || in_time_stamp.is_null() {
        return coreaudio_sys::kAudio_ParamError;
    }

    // SAFETY: the refcon points to the stable CallbackState Box retained by the
    // handle, and CoreAudio serializes render callbacks for this audio unit.
    let state = unsafe { &mut *in_ref_con.cast::<CallbackState>() };
    let frame_count = in_number_frames as usize;
    if frame_count > state.render_buffer.len() {
        state.faults.report_overflow();
        return kAudioUnitErr_TooManyFramesToProcess;
    }

    let byte_count = frame_count.saturating_mul(mem::size_of::<i16>());
    let Ok(byte_count) = u32::try_from(byte_count) else {
        state.faults.report_overflow();
        return kAudioUnitErr_TooManyFramesToProcess;
    };
    let buffer = AudioBuffer {
        mNumberChannels: 1,
        mDataByteSize: byte_count,
        mData: state.render_buffer.as_mut_ptr().cast::<c_void>(),
    };
    let mut buffer_list = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [buffer],
    };

    // SAFETY: the preallocated buffer is large enough for `in_number_frames`,
    // and all callback pointers were validated above.
    let status = unsafe {
        AudioUnitRender(
            state.unit,
            io_action_flags,
            in_time_stamp,
            INPUT_BUS,
            in_number_frames,
            &mut buffer_list,
        )
    };
    if status != 0 {
        state.faults.report_device_error();
        return status;
    }

    let samples = &state.render_buffer[..frame_count];
    match state.sink.try_write(samples) {
        Ok(()) => 0,
        Err(FrameSinkError::Overflow) => {
            state.faults.report_overflow();
            kAudioUnitErr_TooManyFramesToProcess
        }
        Err(FrameSinkError::Closed | FrameSinkError::WriteFailed) => {
            state.faults.report_device_error();
            coreaudio_sys::kAudio_ParamError
        }
    }
}

struct MacOsCoreAudioHandle {
    unit: Option<AudioUnit>,
    callback_state: Option<Box<CallbackState>>,
    initialized: bool,
    started: bool,
}

// SAFETY: SafeAudioCapture serializes ownership operations. CoreAudio is the
// only concurrent callback owner, and callback storage is released only after
// stop + uninitialize + dispose all succeed.
unsafe impl Send for MacOsCoreAudioHandle {}

impl MacOsCoreAudioHandle {
    fn unstarted(unit: AudioUnit) -> Self {
        Self {
            unit: Some(unit),
            callback_state: None,
            initialized: false,
            started: false,
        }
    }

    fn release_unstarted(&mut self) -> Result<(), PortError> {
        debug_assert!(!self.started);
        self.release_after_stop()
    }

    fn release_after_stop(&mut self) -> Result<(), PortError> {
        let Some(unit) = self.unit else {
            self.callback_state = None;
            self.initialized = false;
            return Ok(());
        };

        if self.initialized {
            // SAFETY: the handle still owns the live audio unit.
            check_status(
                unsafe { AudioUnitUninitialize(unit) },
                "audio.coreaudio_uninitialize",
                true,
            )?;
            self.initialized = false;
        }

        // SAFETY: all active rendering has stopped and the unit is no longer
        // initialized. Success is the final native ownership proof.
        check_status(
            unsafe { AudioComponentInstanceDispose(unit) },
            "audio.coreaudio_dispose",
            true,
        )?;
        self.unit = None;
        self.callback_state = None;
        Ok(())
    }

    fn stop_and_release(&mut self) -> Result<(), PortError> {
        let Some(unit) = self.unit else {
            self.callback_state = None;
            self.started = false;
            self.initialized = false;
            return Ok(());
        };

        if self.started {
            // SAFETY: this handle owns the running audio unit.
            check_status(
                unsafe { AudioOutputUnitStop(unit) },
                "audio.coreaudio_stop",
                true,
            )?;
            self.started = false;
        }
        self.release_after_stop()
    }
}

impl AudioBackendHandle for MacOsCoreAudioHandle {
    fn stop(&mut self) -> Result<(), PortError> {
        self.stop_and_release()
    }
}

impl Drop for MacOsCoreAudioHandle {
    fn drop(&mut self) {
        if self.stop_and_release().is_err()
            && let Some(callback_state) = self.callback_state.take()
        {
            // A native release failure means CoreAudio may still hold the
            // callback refcon. Leaking this small state is safer than a
            // use-after-free during process teardown. Normal lifecycle paths
            // retain the handle and retry instead of reaching this branch.
            Box::leak(callback_state);
        }
    }
}

/// Bounded PCM16 WAV writer used by the CoreAudio backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pcm16WavWriterFactory;

impl AudioWriterFactory for Pcm16WavWriterFactory {
    fn create(
        &self,
        partial_path: &Path,
        request: AudioWriterRequest,
    ) -> Result<WriterPipeline, PortError> {
        validate_pcm16_mono(request.source_format)?;
        validate_pcm16_mono(request.target_format)?;
        let ring_capacity = usize::try_from(request.source_format.sample_rate_hz)
            .ok()
            .and_then(|rate| rate.checked_mul(RING_SECONDS))
            .ok_or_else(|| writer_error("audio.writer_capacity", false))?;
        let ring = Arc::new(PcmRing::new(ring_capacity)?);
        let writer_failed = Arc::new(AtomicBool::new(false));

        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(partial_path)
            .map_err(|_| writer_error("audio.writer_open", true))?;
        file.write_all(&wav_header(request.target_format, 0)?)
            .map_err(|_| writer_error("audio.writer_header", true))?;

        let thread_ring = Arc::clone(&ring);
        let thread_failed = Arc::clone(&writer_failed);
        let writer_thread = thread::Builder::new()
            .name("remtene-audio-writer".to_owned())
            .spawn(move || {
                let result = write_pcm_stream(file, &thread_ring, request);
                if result.is_err() {
                    thread_failed.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|_| writer_error("audio.writer_thread", true))?;

        let sink: Arc<dyn AudioFrameSink> = Arc::new(RingFrameSink {
            ring: Arc::clone(&ring),
            writer_failed,
        });
        let writer: Box<dyn AudioArtifactWriter> = Box::new(Pcm16WavWriter {
            ring,
            writer_thread: Some(writer_thread),
            finalize_result: None,
        });
        Ok(WriterPipeline::new(sink, writer))
    }
}

struct RingFrameSink {
    ring: Arc<PcmRing>,
    writer_failed: Arc<AtomicBool>,
}

impl AudioFrameSink for RingFrameSink {
    fn try_write(&self, interleaved_pcm16: &[i16]) -> Result<(), FrameSinkError> {
        if self.writer_failed.load(Ordering::Acquire) {
            return Err(FrameSinkError::WriteFailed);
        }
        self.ring.try_push(interleaved_pcm16)
    }
}

struct PcmRing {
    slots: Box<[AtomicI16]>,
    read_index: AtomicUsize,
    write_index: AtomicUsize,
    closed: AtomicBool,
    cancelled: AtomicBool,
}

impl PcmRing {
    fn new(capacity: usize) -> Result<Self, PortError> {
        if capacity == 0 {
            return Err(writer_error("audio.writer_capacity", false));
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| writer_error("audio.writer_capacity", true))?;
        slots.extend((0..capacity).map(|_| AtomicI16::new(0)));
        Ok(Self {
            slots: slots.into_boxed_slice(),
            read_index: AtomicUsize::new(0),
            write_index: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        })
    }

    fn try_push(&self, samples: &[i16]) -> Result<(), FrameSinkError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(FrameSinkError::Closed);
        }
        if samples.is_empty() {
            return Ok(());
        }

        let write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        if used > self.slots.len() || samples.len() > self.slots.len() - used {
            return Err(FrameSinkError::Overflow);
        }

        for (offset, sample) in samples.iter().copied().enumerate() {
            let slot = write.wrapping_add(offset) % self.slots.len();
            self.slots[slot].store(sample, Ordering::Relaxed);
        }
        self.write_index
            .store(write.wrapping_add(samples.len()), Ordering::Release);
        Ok(())
    }

    fn drain_into(&self, output: &mut [i16]) -> usize {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let available = write.wrapping_sub(read).min(output.len());
        for (offset, output_sample) in output.iter_mut().take(available).enumerate() {
            let slot = read.wrapping_add(offset) % self.slots.len();
            *output_sample = self.slots[slot].load(Ordering::Relaxed);
        }
        if available != 0 {
            self.read_index
                .store(read.wrapping_add(available), Ordering::Release);
        }
        available
    }

    fn is_empty(&self) -> bool {
        self.read_index.load(Ordering::Acquire) == self.write_index.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.close();
    }
}

struct Pcm16WavWriter {
    ring: Arc<PcmRing>,
    writer_thread: Option<JoinHandle<Result<WriterSummary, PortError>>>,
    finalize_result: Option<Result<WriterSummary, PortError>>,
}

impl Pcm16WavWriter {
    fn wake_writer(&self) {
        if let Some(handle) = self.writer_thread.as_ref() {
            handle.thread().unpark();
        }
    }

    fn join_for_finalize(&mut self) -> Result<WriterSummary, PortError> {
        let Some(handle) = self.writer_thread.take() else {
            return self
                .finalize_result
                .clone()
                .unwrap_or_else(|| Err(writer_error("audio.writer_state", false)));
        };
        handle
            .join()
            .unwrap_or_else(|_| Err(writer_error("audio.writer_panic", false)))
    }

    fn join_for_abort(&mut self) {
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
    }
}

impl AudioArtifactWriter for Pcm16WavWriter {
    fn finalize(&mut self) -> Result<WriterSummary, PortError> {
        if let Some(result) = &self.finalize_result {
            return result.clone();
        }
        self.ring.close();
        self.wake_writer();
        let result = self.join_for_finalize();
        self.finalize_result = Some(result.clone());
        result
    }

    fn abort(&mut self) -> Result<(), PortError> {
        self.ring.cancel();
        self.wake_writer();
        self.join_for_abort();
        Ok(())
    }
}

impl Drop for Pcm16WavWriter {
    fn drop(&mut self) {
        self.ring.cancel();
        self.wake_writer();
        self.join_for_abort();
    }
}

fn write_pcm_stream(
    file: File,
    ring: &PcmRing,
    request: AudioWriterRequest,
) -> Result<WriterSummary, PortError> {
    let mut trace = NormalizationTrace::new(request);
    let result = write_pcm_stream_inner(file, ring, request, &mut trace);
    match result {
        Ok(outcome) if outcome.cancelled => {
            trace.cancelled(outcome.summary.frames_written);
            Ok(outcome.summary)
        }
        Ok(outcome) => {
            trace.completed(outcome.summary.frames_written);
            Ok(outcome.summary)
        }
        Err(error) => {
            trace.failed(&error.code);
            Err(error)
        }
    }
}

struct StreamWriteOutcome {
    summary: WriterSummary,
    cancelled: bool,
}

fn write_pcm_stream_inner(
    file: File,
    ring: &PcmRing,
    request: AudioWriterRequest,
    trace: &mut NormalizationTrace,
) -> Result<StreamWriteOutcome, PortError> {
    let mut writer = BufWriter::new(file);
    let outcome = if request.source_format.sample_rate_hz == request.target_format.sample_rate_hz {
        write_passthrough_stream(&mut writer, ring, trace)?
    } else {
        write_resampled_stream(&mut writer, ring, request, trace)?
    };
    if outcome.cancelled {
        return Ok(outcome);
    }

    finalize_wav(
        &mut writer,
        request.target_format,
        outcome.summary.frames_written,
    )?;
    Ok(outcome)
}

fn write_passthrough_stream(
    writer: &mut BufWriter<File>,
    ring: &PcmRing,
    trace: &mut NormalizationTrace,
) -> Result<StreamWriteOutcome, PortError> {
    let mut pcm = [0_i16; 4_096];
    let mut bytes = [0_u8; 8_192];
    let mut frames_written = 0_u64;

    loop {
        if ring.cancelled.load(Ordering::Acquire) {
            return Ok(StreamWriteOutcome {
                summary: WriterSummary { frames_written },
                cancelled: true,
            });
        }
        let count = ring.drain_into(&mut pcm);
        if count != 0 {
            trace.observe_input(count);
            write_pcm16(writer, &pcm[..count], &mut bytes)?;
            frames_written = checked_add_frames(frames_written, count)?;
            continue;
        }
        if ring.closed.load(Ordering::Acquire) && ring.is_empty() {
            break;
        }
        thread::park_timeout(WRITER_POLL_INTERVAL);
    }

    Ok(StreamWriteOutcome {
        summary: WriterSummary { frames_written },
        cancelled: false,
    })
}

fn write_resampled_stream(
    writer: &mut BufWriter<File>,
    ring: &PcmRing,
    request: AudioWriterRequest,
    trace: &mut NormalizationTrace,
) -> Result<StreamWriteOutcome, PortError> {
    let converter = Pcm16SampleRateConverter::new(request.source_format, request.target_format)?;
    let mut input = ConverterInputState {
        ring,
        trace,
        pcm: [0_i16; 4_096],
    };
    let mut output = [0_i16; 4_096];
    let mut bytes = [0_u8; 8_192];
    let mut frames_written = 0_u64;

    loop {
        if ring.cancelled.load(Ordering::Acquire) {
            return Ok(StreamWriteOutcome {
                summary: WriterSummary { frames_written },
                cancelled: true,
            });
        }
        let count = converter.fill(&mut input, &mut output)?;
        if ring.cancelled.load(Ordering::Acquire) {
            return Ok(StreamWriteOutcome {
                summary: WriterSummary { frames_written },
                cancelled: true,
            });
        }
        if count == 0 {
            break;
        }
        write_pcm16(writer, &output[..count], &mut bytes)?;
        frames_written = checked_add_frames(frames_written, count)?;
    }

    Ok(StreamWriteOutcome {
        summary: WriterSummary { frames_written },
        cancelled: false,
    })
}

fn write_pcm16(
    writer: &mut BufWriter<File>,
    samples: &[i16],
    bytes: &mut [u8],
) -> Result<(), PortError> {
    let byte_count = samples
        .len()
        .checked_mul(mem::size_of::<i16>())
        .ok_or_else(|| writer_error("audio.writer_size", false))?;
    if byte_count > bytes.len() {
        return Err(writer_error("audio.writer_buffer", false));
    }
    for (sample, destination) in samples.iter().zip(bytes[..byte_count].chunks_exact_mut(2)) {
        destination.copy_from_slice(&sample.to_le_bytes());
    }
    writer
        .write_all(&bytes[..byte_count])
        .map_err(|_| writer_error("audio.writer_write", true))
}

fn checked_add_frames(frames: u64, count: usize) -> Result<u64, PortError> {
    frames
        .checked_add(count as u64)
        .ok_or_else(|| writer_error("audio.writer_size", false))
}

fn finalize_wav(
    writer: &mut BufWriter<File>,
    format: AudioFormat,
    frames_written: u64,
) -> Result<(), PortError> {
    let data_bytes = frames_written
        .checked_mul(u64::from(format.channels))
        .and_then(|samples| samples.checked_mul(u64::from(format.bits_per_sample / 8)))
        .ok_or_else(|| writer_error("audio.writer_size", false))?;
    let data_bytes =
        u32::try_from(data_bytes).map_err(|_| writer_error("audio.writer_size", false))?;

    writer
        .flush()
        .map_err(|_| writer_error("audio.writer_flush", true))?;
    writer
        .seek(SeekFrom::Start(0))
        .map_err(|_| writer_error("audio.writer_seek", true))?;
    writer
        .write_all(&wav_header(format, data_bytes)?)
        .map_err(|_| writer_error("audio.writer_header", true))?;
    writer
        .flush()
        .map_err(|_| writer_error("audio.writer_flush", true))?;
    let expected_length = WAV_HEADER_BYTES
        .checked_add(u64::from(data_bytes))
        .ok_or_else(|| writer_error("audio.writer_size", false))?;
    writer
        .get_ref()
        .set_len(expected_length)
        .map_err(|_| writer_error("audio.writer_truncate", true))?;
    Ok(())
}

struct Pcm16SampleRateConverter(AudioConverterRef);

impl Pcm16SampleRateConverter {
    fn new(source: AudioFormat, target: AudioFormat) -> Result<Self, PortError> {
        let source_description = pcm16_description(source);
        let target_description = pcm16_description(target);
        let mut converter = ptr::null_mut();
        // SAFETY: both ASBD values are initialized PCM16 descriptions and the
        // out pointer remains valid for the duration of the call.
        let status =
            unsafe { AudioConverterNew(&source_description, &target_description, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(converter_error("audio.resampler_create"));
        }
        let instance = Self(converter);
        instance.set_u32(
            kAudioConverterSampleRateConverterComplexity,
            kAudioConverterSampleRateConverterComplexity_MinimumPhase,
            "audio.resampler_complexity",
        )?;
        instance.set_u32(
            kAudioConverterSampleRateConverterQuality,
            kAudioConverterQuality_High,
            "audio.resampler_quality",
        )?;
        instance.set_u32(
            kAudioConverterPrimeMethod,
            kConverterPrimeMethod_None,
            "audio.resampler_prime_method",
        )?;
        Ok(instance)
    }

    fn set_u32(&self, property: u32, value: u32, code: &str) -> Result<(), PortError> {
        // SAFETY: the converter is live and the property value is an initialized
        // UInt32 with the exact size declared to AudioConverter Services.
        let status = unsafe {
            AudioConverterSetProperty(
                self.0,
                property,
                size_u32::<u32>(),
                (&value as *const u32).cast::<c_void>(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(converter_error(code))
        }
    }

    fn fill(
        &self,
        input: &mut ConverterInputState<'_>,
        output: &mut [i16],
    ) -> Result<usize, PortError> {
        let byte_count = output
            .len()
            .checked_mul(mem::size_of::<i16>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| converter_error("audio.resampler_output_size"))?;
        let mut output_packets = u32::try_from(output.len())
            .map_err(|_| converter_error("audio.resampler_output_size"))?;
        let mut output_buffers = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: byte_count,
                mData: output.as_mut_ptr().cast::<c_void>(),
            }],
        };
        // SAFETY: the callback state and output buffer remain alive for this
        // synchronous conversion call. PCM packets have fixed size and need no
        // packet descriptions.
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.0,
                Some(converter_input_callback),
                (input as *mut ConverterInputState<'_>).cast::<c_void>(),
                &mut output_packets,
                &mut output_buffers,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(converter_error("audio.resampler_convert"));
        }
        usize::try_from(output_packets).map_err(|_| converter_error("audio.resampler_output_size"))
    }
}

impl Drop for Pcm16SampleRateConverter {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the converter and disposes it exactly once.
        let _ = unsafe { AudioConverterDispose(self.0) };
    }
}

struct ConverterInputState<'a> {
    ring: &'a PcmRing,
    trace: &'a mut NormalizationTrace,
    pcm: [i16; 4_096],
}

unsafe extern "C" fn converter_input_callback(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_packet_description: *mut *mut coreaudio_sys::AudioStreamPacketDescription,
    user_data: *mut c_void,
) -> OSStatus {
    if io_number_data_packets.is_null() || io_data.is_null() || user_data.is_null() {
        return coreaudio_sys::kAudio_ParamError;
    }
    // SAFETY: `fill` supplies this exact state for the synchronous converter
    // call and the callback never stores the reference.
    let state = unsafe { &mut *user_data.cast::<ConverterInputState<'_>>() };
    loop {
        if state.ring.cancelled.load(Ordering::Acquire) {
            // SAFETY: pointers were validated above and refer to writable ABI
            // fields owned by AudioConverter Services.
            unsafe { set_converter_eof(io_number_data_packets, io_data, out_packet_description) };
            return 0;
        }
        // SAFETY: the packet-count pointer was validated and is initialized by
        // AudioConverter Services before invoking the callback.
        let requested = unsafe { *io_number_data_packets } as usize;
        let input_capacity = state.pcm.len();
        let count = state
            .ring
            .drain_into(&mut state.pcm[..requested.min(input_capacity)]);
        if count != 0 {
            state.trace.observe_input(count);
            let Some(byte_count) = count
                .checked_mul(mem::size_of::<i16>())
                .and_then(|bytes| u32::try_from(bytes).ok())
            else {
                return coreaudio_sys::kAudio_ParamError;
            };
            // SAFETY: the converter consumes this fixed PCM buffer before the
            // callback is invoked again; all pointers remain valid meanwhile.
            unsafe {
                *io_number_data_packets = count as u32;
                (*io_data).mNumberBuffers = 1;
                (*io_data).mBuffers[0] = AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: byte_count,
                    mData: state.pcm.as_mut_ptr().cast::<c_void>(),
                };
                if !out_packet_description.is_null() {
                    *out_packet_description = ptr::null_mut();
                }
            }
            return 0;
        }
        if state.ring.closed.load(Ordering::Acquire) && state.ring.is_empty() {
            // SAFETY: same validated output pointers as above.
            unsafe { set_converter_eof(io_number_data_packets, io_data, out_packet_description) };
            return 0;
        }
        thread::park_timeout(WRITER_POLL_INTERVAL);
    }
}

unsafe fn set_converter_eof(
    packet_count: *mut u32,
    data: *mut AudioBufferList,
    packet_description: *mut *mut coreaudio_sys::AudioStreamPacketDescription,
) {
    // SAFETY: caller validates the first two pointers; the packet-description
    // pointer is optional by the AudioConverter callback contract.
    unsafe {
        *packet_count = 0;
        (*data).mNumberBuffers = 1;
        (*data).mBuffers[0] = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: 0,
            mData: ptr::null_mut(),
        };
        if !packet_description.is_null() {
            *packet_description = ptr::null_mut();
        }
    }
}

fn pcm16_description(format: AudioFormat) -> AudioStreamBasicDescription {
    let bytes_per_frame = u32::from(format.bits_per_sample / 8) * u32::from(format.channels);
    AudioStreamBasicDescription {
        mSampleRate: f64::from(format.sample_rate_hz),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: u32::from(format.channels),
        mBitsPerChannel: u32::from(format.bits_per_sample),
        mReserved: 0,
    }
}

struct NormalizationTrace {
    request: AudioWriterRequest,
    started_at: Option<Instant>,
    input_frames: u64,
    terminal_emitted: bool,
}

impl NormalizationTrace {
    fn new(request: AudioWriterRequest) -> Self {
        Self {
            request,
            started_at: None,
            input_frames: 0,
            terminal_emitted: false,
        }
    }

    fn observe_input(&mut self, count: usize) {
        self.input_frames = self.input_frames.saturating_add(count as u64);
        if self.started_at.is_some() {
            return;
        }
        self.started_at = Some(Instant::now());
        crate::trace::audio_normalization(
            self.request.session_id,
            "started",
            None,
            None,
            &format!(
                "handoff=local_writer_thread mode={} source_sample_rate={} source_channels={} source_bits={} target_sample_rate={} target_channels={} target_bits={}",
                self.mode(),
                self.request.source_format.sample_rate_hz,
                self.request.source_format.channels,
                self.request.source_format.bits_per_sample,
                self.request.target_format.sample_rate_hz,
                self.request.target_format.channels,
                self.request.target_format.bits_per_sample,
            ),
        );
    }

    fn completed(&mut self, output_frames: u64) {
        self.emit_terminal("completed", output_frames, None);
    }

    fn cancelled(&mut self, output_frames: u64) {
        self.emit_terminal("cancelled", output_frames, None);
    }

    fn failed(&mut self, error_code: &str) {
        self.emit_terminal("failed", 0, Some(error_code));
    }

    fn emit_terminal(&mut self, state: &str, output_frames: u64, error_code: Option<&str>) {
        if self.terminal_emitted {
            return;
        }
        self.terminal_emitted = true;
        let duration_ms = self
            .started_at
            .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        let audio_duration_ms = output_frames.saturating_mul(1_000)
            / u64::from(self.request.target_format.sample_rate_hz);
        crate::trace::audio_normalization(
            self.request.session_id,
            state,
            duration_ms,
            error_code,
            &format!(
                "mode={} source_frames={} target_frames={} audio_duration_ms={} source_sample_rate={} target_sample_rate={}",
                self.mode(),
                self.input_frames,
                output_frames,
                audio_duration_ms,
                self.request.source_format.sample_rate_hz,
                self.request.target_format.sample_rate_hz,
            ),
        );
    }

    fn mode(&self) -> &'static str {
        if self.request.source_format.sample_rate_hz == self.request.target_format.sample_rate_hz {
            "passthrough"
        } else {
            "resample"
        }
    }
}

fn converter_error(code: &str) -> PortError {
    port_error(code, "errors.audio.resampler", true)
}

fn wav_header(format: AudioFormat, data_bytes: u32) -> Result<[u8; 44], PortError> {
    validate_pcm16_mono(format)?;
    let byte_rate = format
        .sample_rate_hz
        .checked_mul(u32::from(format.channels))
        .and_then(|rate| rate.checked_mul(u32::from(format.bits_per_sample / 8)))
        .ok_or_else(|| writer_error("audio.writer_size", false))?;
    let block_align = u16::from(format.channels) * u16::from(format.bits_per_sample / 8);
    let riff_size = data_bytes
        .checked_add(36)
        .ok_or_else(|| writer_error("audio.writer_size", false))?;

    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&u16::from(format.channels).to_le_bytes());
    header[24..28].copy_from_slice(&format.sample_rate_hz.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&u16::from(format.bits_per_sample).to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    Ok(header)
}

fn default_input_device() -> Result<AudioDeviceID, PortError> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device = kAudioObjectUnknown;
    let mut size = size_u32::<AudioDeviceID>();
    // SAFETY: output storage and size match AudioDeviceID exactly.
    check_status(
        unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                ptr::null(),
                &mut size,
                (&mut device as *mut AudioDeviceID).cast::<c_void>(),
            )
        },
        "audio.coreaudio_default_device",
        true,
    )?;
    if device == kAudioObjectUnknown {
        Err(native_error(
            "audio.coreaudio_default_device",
            "errors.audio.device_unavailable",
            true,
        ))
    } else {
        Ok(device)
    }
}

fn device_sample_rate(device: AudioDeviceID) -> Result<u32, PortError> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut sample_rate = 0_f64;
    let mut size = size_u32::<f64>();
    // SAFETY: output storage and size match Float64 exactly.
    check_status(
        unsafe {
            AudioObjectGetPropertyData(
                device,
                &address,
                0,
                ptr::null(),
                &mut size,
                (&mut sample_rate as *mut f64).cast::<c_void>(),
            )
        },
        "audio.coreaudio_sample_rate",
        true,
    )?;
    if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
        return Err(native_error(
            "audio.coreaudio_sample_rate",
            "errors.audio.device_unavailable",
            false,
        ));
    }
    let rounded = sample_rate.round();
    if (sample_rate - rounded).abs() > 0.01 {
        return Err(native_error(
            "audio.coreaudio_sample_rate",
            "errors.audio.device_unavailable",
            false,
        ));
    }
    Ok(rounded as u32)
}

fn validate_pcm16_mono(format: AudioFormat) -> Result<(), PortError> {
    if format.channels != 1
        || format.bits_per_sample != 16
        || !(8_000..=384_000).contains(&format.sample_rate_hz)
    {
        Err(native_error(
            "audio.unsupported_capture_format",
            "errors.audio.invalid_format",
            false,
        ))
    } else {
        Ok(())
    }
}

fn check_status(status: OSStatus, code: &str, retryable: bool) -> Result<(), PortError> {
    if status == 0 {
        Ok(())
    } else {
        Err(status_error(
            status,
            code,
            "errors.audio.device_unavailable",
            retryable,
        ))
    }
}

fn status_error(
    status: OSStatus,
    code: &str,
    safe_message_key: &str,
    retryable: bool,
) -> PortError {
    if status == kAudioUnitErr_Unauthorized || status == kAudioDevicePermissionsError as OSStatus {
        native_error(
            "audio.microphone_permission_denied",
            "errors.permission.microphone_denied",
            false,
        )
    } else {
        native_error(code, safe_message_key, retryable)
    }
}

fn native_error(code: &str, safe_message_key: &str, retryable: bool) -> PortError {
    port_error(code, safe_message_key, retryable)
}

fn writer_error(code: &str, retryable: bool) -> PortError {
    port_error(code, "errors.audio.artifact_io", retryable)
}

const fn size_u32<T>() -> u32 {
    mem::size_of::<T>() as u32
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        path::PathBuf,
        pin::Pin,
        task::{Context, Poll, Waker},
        thread,
        time::Duration,
    };

    use remtene_application::ports::AudioCapture;
    use remtene_domain::SessionId;

    use super::*;

    fn block_on<T>(mut future: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("audio adapter futures must complete synchronously"),
        }
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remtene-coreaudio-writer-test-{}-{}",
                std::process::id(),
                SessionId::new()
            ));
            fs::create_dir_all(&path).expect("test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_format() -> AudioFormat {
        AudioFormat {
            sample_rate_hz: 16_000,
            channels: 1,
            bits_per_sample: 16,
        }
    }

    fn writer_request(source_format: AudioFormat) -> AudioWriterRequest {
        AudioWriterRequest {
            session_id: SessionId::new(),
            source_format,
            target_format: test_format(),
        }
    }

    #[test]
    fn ring_is_bounded_and_preserves_order() {
        let ring = PcmRing::new(4).expect("ring");
        ring.try_push(&[1, 2, 3]).expect("first write");
        assert_eq!(ring.try_push(&[4, 5]), Err(FrameSinkError::Overflow));

        let mut first = [0_i16; 2];
        assert_eq!(ring.drain_into(&mut first), 2);
        assert_eq!(first, [1, 2]);
        ring.try_push(&[4, 5]).expect("wrapped write");

        let mut second = [0_i16; 4];
        assert_eq!(ring.drain_into(&mut second), 3);
        assert_eq!(&second[..3], &[3, 4, 5]);
    }

    #[test]
    fn closed_ring_rejects_callback_writes() {
        let ring = PcmRing::new(4).expect("ring");
        ring.close();
        assert_eq!(ring.try_push(&[1]), Err(FrameSinkError::Closed));
    }

    #[test]
    fn wav_writer_finalizes_pcm16_header_and_samples() {
        let directory = TestDirectory::new();
        let path = directory.path.join("capture.wav.partial");
        let factory = Pcm16WavWriterFactory;
        let mut pipeline = factory
            .create(&path, writer_request(test_format()))
            .expect("writer pipeline");
        pipeline
            .sink
            .try_write(&[-32_768, -1, 0, 1, 32_767])
            .expect("enqueue samples");
        let summary = pipeline.writer.finalize().expect("finalize");
        assert_eq!(summary.frames_written, 5);

        let bytes = fs::read(path).expect("read wav");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 10);
        assert_eq!(bytes.len(), 54);
        assert_eq!(&bytes[44..46], &i16::MIN.to_le_bytes());
        assert_eq!(&bytes[52..54], &i16::MAX.to_le_bytes());
    }

    #[test]
    fn wav_writer_streams_48khz_device_pcm_into_canonical_16khz_wav() {
        let directory = TestDirectory::new();
        let path = directory.path.join("capture-48k.wav.partial");
        let source_format = AudioFormat {
            sample_rate_hz: 48_000,
            channels: 1,
            bits_per_sample: 16,
        };
        let factory = Pcm16WavWriterFactory;
        let mut pipeline = factory
            .create(&path, writer_request(source_format))
            .expect("48 kHz streaming writer");
        let source: Vec<i16> = (0..48_000)
            .map(|frame| {
                let phase = std::f32::consts::TAU * 440.0 * frame as f32 / 48_000.0;
                (phase.sin() * 8_000.0) as i16
            })
            .collect();
        for chunk in source.chunks(1_024) {
            pipeline.sink.try_write(chunk).expect("enqueue 48 kHz PCM");
        }
        let summary = pipeline.writer.finalize().expect("finalize resampled WAV");

        assert!((15_990..=16_010).contains(&summary.frames_written));
        let bytes = fs::read(path).expect("read canonical WAV");
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as u64,
            summary.frames_written * 2
        );
    }

    #[test]
    fn wav_writer_abort_closes_file_without_waiting_for_finalize() {
        let directory = TestDirectory::new();
        let path = directory.path.join("cancelled.wav.partial");
        let factory = Pcm16WavWriterFactory;
        let mut pipeline = factory
            .create(&path, writer_request(test_format()))
            .expect("writer pipeline");
        pipeline.sink.try_write(&[1, 2, 3]).expect("enqueue");
        pipeline.writer.abort().expect("abort");

        fs::remove_file(path).expect("closed writer permits immediate removal");
    }

    #[test]
    #[ignore = "requires a real macOS input device"]
    fn live_default_input_exposes_a_supported_pcm_format() {
        let format = MacOsCoreAudioBackend::default_input_format().expect("default input format");
        validate_pcm16_mono(format).expect("supported format");
    }

    #[test]
    #[ignore = "requires explicit REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE=1 and microphone permission"]
    fn live_half_second_capture_releases_handle_and_cleans_wav() {
        assert_eq!(
            std::env::var("REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE").as_deref(),
            Ok("1"),
            "set REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE=1 explicitly before opening the microphone"
        );

        let directory = TestDirectory::new();
        let source_format = MacOsCoreAudioBackend::default_input_format().expect("input format");
        eprintln!(
            "live source={}Hz/{}ch/{}bit target={}Hz/{}ch/{}bit",
            source_format.sample_rate_hz,
            source_format.channels,
            source_format.bits_per_sample,
            CANONICAL_ASR_AUDIO_FORMAT.sample_rate_hz,
            CANONICAL_ASR_AUDIO_FORMAT.channels,
            CANONICAL_ASR_AUDIO_FORMAT.bits_per_sample,
        );
        let capture_adapter =
            create_default_macos_audio_capture(&directory.path).expect("capture adapter");
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start microphone");
        assert_eq!(capture_adapter.active_capture_count(), 1);
        thread::sleep(Duration::from_millis(500));

        let audio = match block_on(capture_adapter.finish(capture.clone())) {
            Ok(audio) => audio,
            Err(error) => {
                let cleanup = block_on(capture_adapter.cancel(capture));
                panic!("finish failed: {error:?}; cancellation result: {cleanup:?}");
            }
        };
        assert_eq!(audio.format, CANONICAL_ASR_AUDIO_FORMAT);
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert!(audio.duration_ms > 0);

        let artifact = capture_adapter
            .resolve_artifact(&audio.audio_ref)
            .expect("resolve artifact")
            .expect("registered artifact");
        let artifact_path = artifact.path().to_path_buf();
        let bytes = fs::read(&artifact_path).expect("read finalized wav");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().expect("data size"));
        assert!(data_bytes > 0);
        assert_eq!(bytes.len(), WAV_HEADER_BYTES as usize + data_bytes as usize);

        block_on(capture_adapter.cleanup(audio.audio_ref)).expect("cleanup finalized wav");
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        assert!(!artifact_path.exists());
    }

    #[test]
    #[ignore = "requires explicit REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE=1 and microphone permission"]
    fn live_repeated_finish_and_cancel_restart_release_everything() {
        assert_eq!(
            std::env::var("REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE").as_deref(),
            Ok("1"),
            "set REMTENE_RUN_LIVE_MACOS_AUDIO_SMOKE=1 explicitly before opening the microphone"
        );

        let directory = TestDirectory::new();
        let capture_adapter =
            create_default_macos_audio_capture(&directory.path).expect("capture adapter");

        for cycle in 0..2 {
            finish_and_cleanup_live_capture(&capture_adapter, Duration::from_millis(100));
            assert_eq!(
                capture_adapter.active_capture_count(),
                0,
                "completed cycle {cycle} retained a microphone handle"
            );
        }

        let cancelled =
            block_on(capture_adapter.start(SessionId::new())).expect("start cancelled cycle");
        thread::sleep(Duration::from_millis(50));
        block_on(capture_adapter.cancel(cancelled)).expect("cancel live capture");
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);

        finish_and_cleanup_live_capture(&capture_adapter, Duration::from_millis(100));
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        assert_eq!(
            fs::read_dir(&directory.path)
                .expect("read live artifact directory")
                .count(),
            0,
            "repeated lifecycle left an audio artifact behind"
        );
    }

    fn finish_and_cleanup_live_capture(capture_adapter: &SafeAudioCapture, duration: Duration) {
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start microphone");
        thread::sleep(duration);
        let audio = match block_on(capture_adapter.finish(capture.clone())) {
            Ok(audio) => audio,
            Err(error) => {
                let cleanup = block_on(capture_adapter.cancel(capture));
                panic!("finish failed: {error:?}; cancellation result: {cleanup:?}");
            }
        };
        assert!(audio.duration_ms > 0);
        let artifact = capture_adapter
            .resolve_artifact(&audio.audio_ref)
            .expect("resolve live artifact")
            .expect("registered live artifact");
        let bytes = fs::read(artifact.path()).expect("read finalized live wav");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().expect("data size"));
        assert!(data_bytes > 0);
        let path = artifact.path().to_path_buf();
        block_on(capture_adapter.cleanup(audio.audio_ref)).expect("cleanup live wav");
        assert!(!path.exists());
    }
}
