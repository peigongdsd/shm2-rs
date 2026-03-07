use std::io::{self, Read};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use glib::prelude::*;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

#[derive(Clone, Debug)]
enum ListenSpec {
    Tcp { host: String, port: u16 },
    #[cfg(target_os = "linux")]
    Vsock { cid: u32, port: u32 },
}

fn parse_listen(spec: &str) -> Result<ListenSpec, String> {
    if let Some(rest) = spec.strip_prefix("tcp://") {
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or("tcp listen must be tcp://host:port")?;
        let host = if host.is_empty() { "0.0.0.0" } else { host };
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid tcp port")?;
        return Ok(ListenSpec::Tcp {
            host: host.to_string(),
            port,
        });
    }
    #[cfg(target_os = "linux")]
    if let Some(rest) = spec.strip_prefix("vsock://") {
        let (cid, port) = rest
            .split_once(':')
            .ok_or("vsock listen must be vsock://cid:port")?;
        let cid = cid.parse::<u32>().map_err(|_| "invalid vsock cid")?;
        let port = port
            .parse::<u32>()
            .map_err(|_| "invalid vsock port")?;
        return Ok(ListenSpec::Vsock { cid, port });
    }
    Err("listen must be tcp://host:port or vsock://cid:port".to_string())
}

fn normalize_pipeline(input: &str) -> String {
    let mut normalized = input.replace("\\!", "!");
    normalized = normalize_caps_filters(&normalized);
    normalized
}

fn normalize_caps_filters(input: &str) -> String {
    // Accept gst-launch-style caps tokens after '!' and rewrite to capsfilter
    // so gst_parse_launch doesn't misinterpret them as element names.
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return input.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "!" && i + 1 < tokens.len() {
            let next = tokens[i + 1];
            let is_caps = next.starts_with("video/")
                || next.starts_with("audio/")
                || next.starts_with("text/")
                || next.starts_with("application/");
            if is_caps {
                out.push("!".to_string());
                out.push("capsfilter".to_string());
                out.push(format!("caps={}", next));
                i += 2;
                continue;
            }
        }
        out.push(tokens[i].to_string());
        i += 1;
    }
    out.join(" ")
}

#[derive(Clone, Copy, Debug)]
struct AppSrcConfig {
    max_buffers: u64,
    max_bytes: u64,
    max_time_ns: u64,
    block: bool,
    leaky: gst_app::AppLeakyType,
}

impl Default for AppSrcConfig {
    fn default() -> Self {
        Self {
            max_buffers: 8,
            max_bytes: 0,
            max_time_ns: 0,
            block: true,
            leaky: gst_app::AppLeakyType::None,
        }
    }
}

fn parse_args() -> Result<(ListenSpec, String, u64, String, Option<String>, bool, AppSrcConfig), String> {
    let mut listen = "tcp://0.0.0.0:5555".to_string();
    let mut shm_path: Option<String> = None;
    let mut shm_size: u64 = 64 * 1024 * 1024;
    let mut input: Option<String> = None;
    let mut splash: Option<String> = None;
    let mut deep_copy = true;
    let mut appsrc_cfg = AppSrcConfig::default();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen = args
                    .next()
                    .ok_or("--listen requires a value")?;
            }
            "--shm-path" => {
                shm_path = Some(args.next().ok_or("--shm-path requires a value")?);
            }
            "--shm-size" => {
                let value = args.next().ok_or("--shm-size requires a value")?;
                shm_size = value
                    .parse::<u64>()
                    .map_err(|_| "--shm-size must be a u64")?;
            }
            "--input" => {
                input = Some(args.next().ok_or("--input requires a pipeline string")?);
            }
            "--splash" => {
                splash = Some(args.next().ok_or("--splash requires a pipeline string")?);
            }
            "--no-deep-copy" => {
                deep_copy = false;
            }
            "--appsrc-max-buffers" => {
                let value = args
                    .next()
                    .ok_or("--appsrc-max-buffers requires a value")?;
                appsrc_cfg.max_buffers = value
                    .parse::<u64>()
                    .map_err(|_| "--appsrc-max-buffers must be a u64")?;
            }
            "--appsrc-max-bytes" => {
                let value = args
                    .next()
                    .ok_or("--appsrc-max-bytes requires a value")?;
                appsrc_cfg.max_bytes = value
                    .parse::<u64>()
                    .map_err(|_| "--appsrc-max-bytes must be a u64")?;
            }
            "--appsrc-max-time" => {
                let value = args
                    .next()
                    .ok_or("--appsrc-max-time requires a value (ns)")?;
                appsrc_cfg.max_time_ns = value
                    .parse::<u64>()
                    .map_err(|_| "--appsrc-max-time must be a u64 (ns)")?;
            }
            "--appsrc-block" => {
                appsrc_cfg.block = true;
            }
            "--appsrc-no-block" => {
                appsrc_cfg.block = false;
            }
            "--appsrc-leaky" => {
                let value = args
                    .next()
                    .ok_or("--appsrc-leaky requires none|upstream|downstream")?;
                appsrc_cfg.leaky = match value.as_str() {
                    "none" => gst_app::AppLeakyType::None,
                    "upstream" => gst_app::AppLeakyType::Upstream,
                    "downstream" => gst_app::AppLeakyType::Downstream,
                    _ => return Err("--appsrc-leaky must be none|upstream|downstream".to_string()),
                };
            }
            "--help" | "-h" => {
                return Err("help".to_string());
            }
            _ => return Err(format!("unknown arg: {arg}")),
        }
    }

    let shm_path = shm_path.ok_or("--shm-path is required")?;
    let input = input.ok_or("--input is required")?;
    let listen = parse_listen(&listen)?;

    Ok((
        listen,
        shm_path,
        shm_size,
        normalize_pipeline(&input),
        splash.map(|p| normalize_pipeline(&p)),
        deep_copy,
        appsrc_cfg,
    ))
}

fn usage() {
    eprintln!(
        "Usage: shm2_relayd --shm-path <path> [--shm-size <bytes>] --input <pipeline> [--splash <pipeline>] [--listen tcp://0.0.0.0:5555|vsock://CID:PORT] [--no-deep-copy] [--appsrc-max-buffers <n>] [--appsrc-max-bytes <n>] [--appsrc-max-time <ns>] [--appsrc-block|--appsrc-no-block] [--appsrc-leaky none|upstream|downstream]"
    );
}

fn set_pipeline_time(pipeline: &gst::Pipeline, base_time: Option<gst::ClockTime>) {
    let clock = gst::SystemClock::obtain();
    pipeline.use_clock(Some(&clock));
    if let Some(bt) = base_time {
        pipeline.set_base_time(bt);
    } else if let Some(now) = clock.time() {
        pipeline.set_base_time(now);
    }
    pipeline.set_start_time(gst::ClockTime::NONE);
}

fn output_pipeline_create(
    shm_path: &str,
    shm_size: u64,
    appsrc_cfg: AppSrcConfig,
) -> Result<(gst::Pipeline, gst_app::AppSrc), gst::glib::Error> {
    let pipeline_str = format!(
        "appsrc name=appsrc is-live=true format=time stream-type=stream ! queue max-size-buffers=8 max-size-bytes=0 max-size-time=0 leaky=downstream ! shm2sink shm-path={} shm-size={}",
        shm_path, shm_size
    );
    let element = gst::parse::launch(&pipeline_str)?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| gst::glib::Error::new(gst::CoreError::Failed, "output pipeline must be a pipeline"))?;

    set_pipeline_time(&pipeline, None);

    let appsrc = pipeline
        .by_name("appsrc")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| gst::glib::Error::new(gst::CoreError::Failed, "appsrc not found"))?;

    appsrc.set_property("is-live", true);
    appsrc.set_property("format", gst::Format::Time);
    appsrc.set_property("stream-type", gst_app::AppStreamType::Stream);
    appsrc.set_property("block", appsrc_cfg.block);
    appsrc.set_property("max-buffers", appsrc_cfg.max_buffers);
    appsrc.set_property("max-bytes", appsrc_cfg.max_bytes);
    appsrc.set_property(
        "max-time",
        gst::ClockTime::from_nseconds(appsrc_cfg.max_time_ns),
    );
    appsrc.set_property("leaky-type", appsrc_cfg.leaky);

    Ok((pipeline, appsrc))
}

#[derive(Clone)]
struct UpstreamPipeline {
    pipeline: gst::Pipeline,
    caps_set: Arc<AtomicBool>,
}

fn backend_pipeline_create(
    name: &str,
    pipeline_str: &str,
    appsrc: &gst_app::AppSrc,
    base_time: Option<gst::ClockTime>,
    deep_copy: bool,
) -> Result<UpstreamPipeline, gst::glib::Error> {
    let element = gst::parse::launch(pipeline_str)?;
    let pipeline = match element.clone().downcast::<gst::Pipeline>() {
        Ok(p) => p,
        Err(element) => {
            let pipeline = gst::Pipeline::with_name(name);
            pipeline
                .add(&element)
                .map_err(|err| gst::glib::Error::new(gst::CoreError::Failed, &err.to_string()))?;
            pipeline
        }
    };

    set_pipeline_time(&pipeline, base_time);

    let src_pad = pipeline
        .find_unlinked_pad(gst::PadDirection::Src)
        .ok_or_else(|| gst::glib::Error::new(gst::CoreError::Failed, "no unlinked src pad"))?;

    let appsrc_weak = appsrc.downgrade();
    let caps_set = Arc::new(AtomicBool::new(false));
    let caps_set_cb = caps_set.clone();
    let callbacks = gst_app::AppSinkCallbacks::builder()
        .new_sample(move |sink| {
            let appsrc = match appsrc_weak.upgrade() {
                Some(a) => a,
                None => return Err(gst::FlowError::Flushing),
            };
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            if !caps_set_cb.load(Ordering::Relaxed) {
                if let Some(caps) = sample.caps() {
                    let caps = caps.to_owned();
                    appsrc.set_caps(Some(&caps));
                    caps_set_cb.store(true, Ordering::Relaxed);
                }
            }
            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
            let buffer = if deep_copy {
                buffer.copy_deep().map_err(|_| gst::FlowError::Error)?
            } else {
                // Shallow copy: keep GstMemory refs without duplicating payloads.
                buffer
                    .copy_region(gst::BUFFER_COPY_ALL, ..)
                    .map_err(|_| gst::FlowError::Error)?
            };
            appsrc.push_buffer(buffer).map_err(|_| gst::FlowError::Error)?;
            Ok(gst::FlowSuccess::Ok)
        })
        .build();
    let appsink = gst_app::AppSink::builder()
        .callbacks(callbacks)
        .drop(true)
        .max_buffers(4)
        .build();
    appsink.set_property("sync", false);

    pipeline
        .add(&appsink)
        .map_err(|err| gst::glib::Error::new(gst::CoreError::Failed, &err.to_string()))?;
    let sink_pad = appsink
        .static_pad("sink")
        .ok_or_else(|| gst::glib::Error::new(gst::CoreError::Failed, "appsink sink pad missing"))?;
    src_pad.link(&sink_pad).map_err(|_| {
        gst::glib::Error::new(gst::CoreError::Failed, "failed to link appsink")
    })?;

    Ok(UpstreamPipeline { pipeline, caps_set })
}

fn run_tcp_listener(
    spec: &ListenSpec,
    main_ctx: glib::MainContext,
    appsrc: gst_app::AppSrc,
    input_pipeline: Arc<UpstreamPipeline>,
    splash_pipeline: Option<Arc<UpstreamPipeline>>,
) -> io::Result<()> {
    let ListenSpec::Tcp { host, port } = spec else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not tcp"));
    };
    let addr = format!("{}:{}", host, port);
    let listener = TcpListener::bind(addr)?;
    let count = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(err) => {
                eprintln!("accept error: {err}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        println!("[shm2_relayd] client connected {peer}");
        let current = count.fetch_add(1, Ordering::SeqCst) + 1;
        notify_client_count(
            &main_ctx,
            &appsrc,
            &input_pipeline,
            splash_pipeline.as_ref(),
            current,
        );

        let main_ctx = main_ctx.clone();
        let input_pipeline = input_pipeline.clone();
        let splash_pipeline = splash_pipeline.clone();
        let appsrc = appsrc.clone();
        let count_clone = count.clone();
        thread::spawn(move || {
            let _ = hold_connection(stream, &peer);
            let current = count_clone.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
            notify_client_count(
                &main_ctx,
                &appsrc,
                &input_pipeline,
                splash_pipeline.as_ref(),
                current,
            );
            println!("[shm2_relayd] client disconnected {peer}");
        });
    }

    Ok(())
}

fn hold_connection(mut stream: std::net::TcpStream, peer: &str) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    let mut pending = Vec::with_capacity(1024);
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line = pending.drain(..=pos).collect::<Vec<u8>>();
            let msg = String::from_utf8_lossy(&line);
            let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
            if !msg.is_empty() {
                println!("[shm2_relayd] {peer}: {msg}");
            }
        }
        if pending.len() >= 1024 {
            let chunk = pending.drain(..).collect::<Vec<u8>>();
            let msg = String::from_utf8_lossy(&chunk);
            let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
            if !msg.is_empty() {
                println!("[shm2_relayd] {peer}: {msg}");
            }
        }
    }
    if !pending.is_empty() {
        let msg = String::from_utf8_lossy(&pending);
        let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
        if !msg.is_empty() {
            println!("[shm2_relayd] {peer}: {msg}");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_vsock_listener(
    cid: u32,
    port: u32,
    main_ctx: glib::MainContext,
    appsrc: gst_app::AppSrc,
    input_pipeline: Arc<UpstreamPipeline>,
    splash_pipeline: Option<Arc<UpstreamPipeline>>,
) -> io::Result<()> {
    use libc::{AF_VSOCK, SOCK_STREAM};
    use std::mem::size_of;

    let fd = unsafe { libc::socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let addr = libc::sockaddr_vm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0u8; 4],
    };

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_vm>() as u32,
        )
    };
    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::listen(fd, 64) } < 0 {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }

    let count = Arc::new(AtomicUsize::new(0));

    loop {
        let conn = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn < 0 {
            eprintln!("vsock accept error: {}", io::Error::last_os_error());
            continue;
        }
        println!("[shm2_relayd] client connected vsock:{cid}:{port}");
        let current = count.fetch_add(1, Ordering::SeqCst) + 1;
        notify_client_count(
            &main_ctx,
            &appsrc,
            &input_pipeline,
            splash_pipeline.as_ref(),
            current,
        );

        let main_ctx = main_ctx.clone();
        let input_pipeline = input_pipeline.clone();
        let splash_pipeline = splash_pipeline.clone();
        let appsrc = appsrc.clone();
        let count_clone = count.clone();
        thread::spawn(move || {
            let _ = hold_vsock_connection(conn, cid, port);
            let current = count_clone.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
            notify_client_count(
                &main_ctx,
                &appsrc,
                &input_pipeline,
                splash_pipeline.as_ref(),
                current,
            );
            println!("[shm2_relayd] client disconnected vsock:{cid}:{port}");
        });
    }
}

#[cfg(target_os = "linux")]
fn hold_vsock_connection(fd: RawFd, cid: u32, port: u32) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    let mut pending = Vec::with_capacity(1024);
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n == 0 {
            break;
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        let n = n as usize;
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line = pending.drain(..=pos).collect::<Vec<u8>>();
            let msg = String::from_utf8_lossy(&line);
            let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
            if !msg.is_empty() {
                println!("[shm2_relayd] vsock:{cid}:{port}: {msg}");
            }
        }
        if pending.len() >= 1024 {
            let chunk = pending.drain(..).collect::<Vec<u8>>();
            let msg = String::from_utf8_lossy(&chunk);
            let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
            if !msg.is_empty() {
                println!("[shm2_relayd] vsock:{cid}:{port}: {msg}");
            }
        }
    }
    if !pending.is_empty() {
        let msg = String::from_utf8_lossy(&pending);
        let msg = msg.trim_end_matches(['\r', '\n'].as_ref());
        if !msg.is_empty() {
            println!("[shm2_relayd] vsock:{cid}:{port}: {msg}");
        }
    }
    unsafe { libc::close(fd) };
    Ok(())
}

fn notify_client_count(
    main_ctx: &glib::MainContext,
    appsrc: &gst_app::AppSrc,
    input_pipeline: &UpstreamPipeline,
    splash_pipeline: Option<&Arc<UpstreamPipeline>>,
    count: usize,
) {
    let appsrc = appsrc.clone();
    let input_pipeline = input_pipeline.clone();
    let splash_pipeline = splash_pipeline.cloned();
    main_ctx.invoke(move || {
        if count > 0 {
            if let Some(splash) = &splash_pipeline {
                let _ = splash.pipeline.set_state(gst::State::Null);
                splash.caps_set.store(false, Ordering::Relaxed);
            }
            let _ = appsrc.set_caps(None);
            flush_appsrc(&appsrc);
            let _ = input_pipeline.pipeline.set_state(gst::State::Playing);
            if let Some(splash) = &splash_pipeline {
                let _ = splash.pipeline.set_state(gst::State::Null);
            }
        } else {
            let _ = input_pipeline.pipeline.set_state(gst::State::Null);
            input_pipeline.caps_set.store(false, Ordering::Relaxed);
            let _ = appsrc.set_caps(None);
            flush_appsrc(&appsrc);
            if let Some(splash) = &splash_pipeline {
                let _ = splash.pipeline.set_state(gst::State::Playing);
            }
        }
    });
}

fn flush_appsrc(appsrc: &gst_app::AppSrc) {
    if let Some(src_pad) = appsrc.static_pad("src") {
        let _ = src_pad.send_event(gst::event::FlushStart::new());
        let _ = src_pad.send_event(gst::event::FlushStop::new(false));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Err(err) = gst::init() {
        return Err(Box::new(err));
    }

    let (listen, shm_path, shm_size, input, splash, deep_copy, appsrc_cfg) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            if e == "help" {
                usage();
                return Ok(());
            }
            eprintln!("error: {e}");
            usage();
            return Err(e.into());
        }
    };

    let (output_pipeline, appsrc) = output_pipeline_create(&shm_path, shm_size, appsrc_cfg)?;
    let base_time = output_pipeline.base_time();

    let input_pipeline =
        backend_pipeline_create("input-pipeline", &input, &appsrc, base_time, deep_copy)?;
    let splash_pipeline = if let Some(p) = splash.as_deref() {
        Some(backend_pipeline_create(
            "splash-pipeline",
            p,
            &appsrc,
            base_time,
            deep_copy,
        )?)
    } else {
        None
    };

    let main_loop = glib::MainLoop::new(None, false);
    let main_loop_clone = main_loop.clone();

    let bus = output_pipeline.bus().expect("pipeline bus");
    let _bus_watch = bus.add_watch(move |_, msg| {
        match msg.view() {
            gst::MessageView::Error(err) => {
                let error = err.error();
                eprintln!("output pipeline error: {error}");
                main_loop_clone.quit();
                return glib::ControlFlow::Break;
            }
            gst::MessageView::Eos(..) => {
                main_loop_clone.quit();
                return glib::ControlFlow::Break;
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    })?;

    output_pipeline.set_state(gst::State::Playing)?;
    if let Some(splash) = &splash_pipeline {
        splash.pipeline.set_state(gst::State::Playing)?;
    }

    let input_pipeline = Arc::new(input_pipeline);
    let splash_pipeline = splash_pipeline.map(Arc::new);
    let main_ctx = glib::MainContext::default();

    thread::spawn({
        let listen = listen.clone();
        let main_ctx = main_ctx.clone();
        let input_pipeline = input_pipeline.clone();
        let splash_pipeline = splash_pipeline.clone();
        let appsrc = appsrc.clone();
        move || match listen {
            ListenSpec::Tcp { .. } => {
                if let Err(err) =
                    run_tcp_listener(&listen, main_ctx, appsrc, input_pipeline, splash_pipeline)
                {
                    eprintln!("tcp listener error: {err}");
                }
            }
            #[cfg(target_os = "linux")]
            ListenSpec::Vsock { cid, port } => {
                if let Err(err) =
                    run_vsock_listener(cid, port, main_ctx, appsrc, input_pipeline, splash_pipeline)
                {
                    eprintln!("vsock listener error: {err}");
                }
            }
        }
    });

    main_loop.run();

    let _ = output_pipeline.set_state(gst::State::Null);
    let _ = input_pipeline.pipeline.set_state(gst::State::Null);
    if let Some(splash) = &splash_pipeline {
        let _ = splash.pipeline.set_state(gst::State::Null);
    }

    Ok(())
}
