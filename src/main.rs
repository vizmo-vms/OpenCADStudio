#![allow(non_snake_case)]
// On Windows release builds, hide the console window the OS would
// otherwise spawn alongside the GUI. Debug builds keep stdout/stderr
// attached so eprintln! / panics stay visible while developing.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use OpenCADStudio::app;
#[cfg(not(target_arch = "wasm32"))]
use OpenCADStudio::{cli, io, mcp, rest};
#[cfg(target_arch = "wasm32")]
use OpenCADStudio::sys;

/// SecurePlan CAD cannot hold its one-window lock or serve other launches.
#[cfg(feature = "secureplan")]
const UNAVAILABLE: &str = "SecurePlan CAD cannot use its settings folder, so it cannot make sure only one copy is open. Check that your user account can write to its application settings folder, then start SecurePlan CAD again.";

fn main() -> iced::Result {
    // Web (wasm) uses the single-window entry; native uses the multi-window
    // daemon. Trunk calls `main` from its generated JS bootstrap. The web build
    // takes no CLI args, so it skips parsing entirely.
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
        // After the console hook so the chained panic mirror keeps it; also
        // installs the log-facade listener that surfaces wgpu/naga errors as
        // a copyable on-page banner (#414 — an empty canvas otherwise gives
        // reporters nothing to paste).
        sys::web_diag::init();
        return app::run_web();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // SecurePlan CAD's update helper (DSK-07) runs before anything else.
        #[cfg(feature = "secureplan")]
        if let Some(code) = OpenCADStudio::app::secureplan::update_helper::main_hook() {
            std::process::exit(code);
        }
        use clap::Parser;
        let args = cli::Cli::parse();

        // SecurePlan CAD runs no automation listener and loads no plugins
        // (DSK-02): refuse those modes before anything else starts.
        #[cfg(feature = "secureplan")]
        {
            use OpenCADStudio::app::secureplan::hardening;
            if let Some(refusal) = hardening::headless_automation_refusal(args.mcp, args.serve, args.http.is_some(), args.sync_mcp_schemas) {
                eprintln!("{refusal}");
                std::process::exit(2);
            }
            if args.ocs_plugin_runner.is_some() {
                eprintln!("{}", hardening::DISABLED);
                std::process::exit(2);
            }
            // No drawing is read or written outside a session (DSK-08).
            if let Some(refusal) =
                hardening::headless_file_refusal(args.export.is_some(), args.dwg_thumbnail.is_some(), args.script.is_some())
            {
                eprintln!("{refusal}");
                std::process::exit(2);
            }
        }
        // Test builds only: stdin pairing injection for the smoke tests.
        #[cfg(feature = "secureplan-test")]
        OpenCADStudio::app::secureplan::testdriver::start();
        // A `secureplan-cad:` launch URL (Windows and Linux pass it as an
        // argument) is used by this process only: it is never opened as a
        // file or forwarded as an argument (DSK-04).
        #[cfg(feature = "secureplan")]
        let (args, launch_urls) = {
            let mut args = args;
            let (urls, files) = OpenCADStudio::app::secureplan::handoff::split_launch_args(std::mem::take(&mut args.files));
            args.files = files;
            (args, urls)
        };
        // Launch URLs reach a running copy over the authenticated per-user
        // channel only; when none answers, this instance serves them. (The
        // unauthenticated single-instance hand-off is off in SecurePlan CAD.)
        // One SecurePlan CAD window: a copy that handed its URLs over exits.
        #[cfg(feature = "secureplan")]
        if OpenCADStudio::app::secureplan::handoff::forward_launches(&launch_urls) {
            return Ok(());
        }
        // Started by the macOS launcher while a link is on its way over the
        // hand-off: no editor window until a session opens, as for a link
        // start (BRG-02). The flag carries no pairing data.
        #[cfg(feature = "secureplan")]
        let awaiting_launch = args.secureplan_awaiting_launch && launch_urls.is_empty();
        #[cfg(feature = "secureplan")]
        if awaiting_launch {
            OpenCADStudio::app::secureplan::begin_awaiting_launch();
        }
        // Started by links alone: a link no website may use opens nothing;
        // otherwise the editor stays hidden until a session opens (DSK-04).
        #[cfg(feature = "secureplan")]
        if !launch_urls.is_empty()
            && args.files.is_empty()
            && !args.new
            && args.script.is_none()
            && !OpenCADStudio::app::secureplan::begin_cold_start(&launch_urls)
        {
            return Ok(());
        }

        // GPU probe child: exercise one backend offscreen, print one JSON
        // line on success and exit 0/1. This must run before any logging/GUI
        // setup; the parent interprets any non-zero exit (including a driver
        // abort) as "this backend is unusable".
        if let Some(which) = &args.gpu_probe {
            OpenCADStudio::gpu_backend::run_probe_child(which);
        }

        // Plugin runner mode: the host spawns itself with this hidden flag to
        // load a plugin cdylib in an isolated process. Hand off immediately so
        // the child never touches GUI state.
        if let Some(runner_args) = &args.ocs_plugin_runner {
            if runner_args.len() != 2 {
                eprintln!("--ocs-plugin-runner expects <socket> <cdylib>");
                std::process::exit(1);
            }
            let socket = &runner_args[0];
            let cdylib = std::path::Path::new(&runner_args[1]);
            if let Err(e) = ocs_plugin_api::runner::run(socket, cdylib) {
                eprintln!("[runner] fatal: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }

        // Thumbnail mode: the OS file-manager thumbnailer invokes us as
        // `--dwg-thumbnail <in> <out> <size>`. Extract the DWG's embedded
        // preview to a PNG and exit — never touch the GUI.
        if let Some(a) = &args.dwg_thumbnail {
            let size = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(256);
            let ok = io::thumbnail::extract_to_png(
                std::path::Path::new(&a[0]),
                std::path::Path::new(&a[1]),
                size,
            );
            std::process::exit(if ok { 0 } else { 1 });
        }

        if args.sync_mcp_schemas {
            let synced = mcp::sync_agent_tool_schemas();
            if synced {
                println!("Successfully synchronized OpenCADStudio MCP schemas to ~/.gemini/antigravity/mcp/opencadstudio/");
            } else {
                eprintln!("Antigravity MCP directory ~/.gemini/antigravity/mcp/ not found; skipped sync.");
            }
            return Ok(());
        }

        // MCP is a client-neutral local entry point. It uses only stdin,
        // stdout and the authenticated GUI bridge, so it must run before any
        // logging or graphics setup can write to the protocol stream.
        if args.mcp {
            mcp::run();
            return Ok(());
        }

        // Crash log. A release build hides the console, so before this a
        // panic ended the process with nothing on screen and nothing on disk
        // (#635, #845). Installed after the child-process handoffs above: a
        // GPU probe aborting is how an unusable backend is detected, not a
        // crash worth filing.
        // SecurePlan CAD writes no crash log: a panic message can carry drawing
        // content, even for a panic its import catches (DSK-02).
        #[cfg(not(feature = "secureplan"))]
        OpenCADStudio::sys::crash_log::install();

        // Opt-in logging. `--log LEVEL` seeds RUST_LOG; the subscriber then
        // surfaces wgpu / iced / winit diagnostics that are otherwise silent.
        if let Some(level) = &args.log {
            std::env::set_var("RUST_LOG", level);
        }
        if std::env::var_os("RUST_LOG").is_some() {
            let _ = env_logger::try_init();
        }

        // GPU backend selection. Explicit `--backend` wins; `--safe-mode`
        // forces GL for flaky drivers. On Windows the preference order starts
        // with DX12/Vulkan so the AMD OpenGL ICD (atio6axx.dll) is never
        // touched at startup — it access-violates on some hybrid-GPU laptops
        // before any window appears (#55). An already-set WGPU_BACKEND always
        // wins.
        //
        // Older GPUs without usable DirectX 12 Feature Level 12_0 or working
        // Vulkan (e.g. legacy Intel iGPUs), and old GLES drivers whose shader
        // compiler rejects iced's own shaders, crash at startup or on the
        // first drawing viewport instead. `resolve_gpu` probes each candidate
        // backend in an isolated child process and selects the first one that
        // survives a real offscreen render; a crash sentinel skips backends
        // that died with the previous run.
        let gpu = OpenCADStudio::gpu_backend::resolve_gpu(args.backend.as_deref(), args.safe_mode);

        // Headless modes exit without ever creating a window.
        if let Some(port) = args.http {
            if args.files.is_empty() {
                rest::serve(port);
                return Ok(());
            }
            // Files + --http: boot the GUI and host the loopback REST channel
            // on this very process, so a client can open a drawing, let the
            // person pick sample entities, and read them back with
            // get_selection (see app::control::http_bridge).
            rest::set_gui_http_port(port);
        }
        if args.serve {
            // `app::serve` reads --port itself from the raw args.
            app::serve();
            return Ok(());
        }
        if let Some(io) = &args.export {
            // clap enforces exactly two values for --export.
            let code = app::export_headless(&io[0], &io[1], args.target_version.as_deref());
            std::process::exit(code);
        }

        // Single instance: a double-clicked drawing belongs as a tab in the
        // editor that is already open, not in a second copy of the app.
        //
        // The gate is POSITIONAL, and that is the point: every headless mode
        // has already returned above — the plugin runner, which is this same
        // binary re-spawning itself, most of all. A flag list here would rot
        // the first time a mode is added; a position cannot.
        //
        // SecurePlan CAD keeps one window: the primary holds a per-user lock
        // for its lifetime. Any other launch hands its URLs (or, without one,
        // a request to show the window) to the primary over the authenticated
        // per-user channel and exits, retrying while the primary is still
        // starting. With no primary (a crash's stale descriptor included)
        // this launch takes the lock and is the primary.
        #[cfg(feature = "secureplan")]
        let primary_lock = {
            use OpenCADStudio::app::secureplan::handoff::{self, Claim, Request};
            let requests: Vec<Request> = if awaiting_launch {
                Vec::new()
            } else if launch_urls.is_empty() {
                vec![Request::Focus]
            } else {
                launch_urls.iter().cloned().map(Request::Launch).collect()
            };
            match handoff::claim_window(&requests) {
                Claim::Primary(lock) => lock,
                Claim::Forwarded => return Ok(()),
                // The launcher that started this awaiting copy keeps handing
                // its link over, and starts another copy if it must.
                Claim::Unanswered if awaiting_launch => return Ok(()),
                Claim::Unanswered => {
                    handoff::report_start_problem(
                        "SecurePlan CAD is already open but is not responding. Wait a moment and try again. If it stays unresponsive, quit SecurePlan CAD and start it again.",
                    );
                    std::process::exit(1);
                }
                Claim::Unavailable => {
                    handoff::report_start_problem(UNAVAILABLE);
                    std::process::exit(1);
                }
            }
        };
        if !args.new_instance {
            if let io::single_instance::Claim::Existing(stream) = io::single_instance::claim() {
                // Only bare files forward. `--read-only` / `--script` / `--new`
                // configure the whole editor rather than a tab, so they always
                // get a process of their own.
                let plain_open = !args.read_only
                    && args.script.is_none()
                    && !args.new
                    && !args.files.is_empty()
                    // --http hosts its REST channel in this very process, so
                    // the drawing must open here too, never in the existing
                    // editor.
                    && args.http.is_none();
                if plain_open && io::single_instance::handoff(stream, &args.files) {
                    return Ok(());
                }
                // Nothing to forward, or the far end never acknowledged: fall
                // through and boot our own window. We hold no listener, so the
                // editor that owns the port keeps serving.
            }
        }

        // This instance receives later launches, and uses its own once.
        #[cfg(feature = "secureplan")]
        if OpenCADStudio::app::secureplan::handoff::start_primary(primary_lock, launch_urls).is_err() {
            OpenCADStudio::app::secureplan::handoff::report_start_problem(UNAVAILABLE);
            std::process::exit(1);
        }

        // GUI: stash the startup config for `app::boot` to pick up.
        let script_lines = args
            .script
            .as_ref()
            .map(|p| match std::fs::read_to_string(p) {
                Ok(text) => text
                    .lines()
                    .map(str::trim)
                    // Blank script rows are significant: they submit Enter to
                    // the active command (for example, accepting a preview).
                    .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
                    .map(str::to_string)
                    .collect(),
                Err(e) => {
                    // SecurePlan CAD never writes file paths to its output.
                    eprintln!("--script: cannot read {}: {e}", io::diagnostic_path(p));
                    Vec::new()
                }
            })
            .unwrap_or_default();
        let gpu_fallback_notice = gpu
            .reason
            .map(|reason| OpenCADStudio::gpu_backend::fallback_notice(&reason));
        // The probe enables the packed renderer automatically on GPUs without
        // shader storage buffers; an explicit `--compat-renderer` does the
        // same without the notice.
        let compat_renderer = args.compat_renderer || gpu.compat_renderer;
        let gpu_compat_auto = !args.compat_renderer && gpu.compat_renderer;
        let _ = cli::GUI_CONFIG.set(cli::GuiConfig {
            files: if args.new { Vec::new() } else { args.files },
            new: args.new,
            read_only: args.read_only,
            compat_renderer,
            script_lines,
            gpu_fallback_notice,
            gpu_compat_auto,
        });

        // Register (or refresh) the freedesktop DWG thumbnailer so file managers
        // show OCS-embedded previews. Idempotent, best-effort, no consent step —
        // it only points a `.thumbnailer` at this same binary's `--dwg-thumbnail`
        // mode. Silently ignored on failure or non-Linux.
        io::file_association::install_thumbnailer();

        // Crash sentinel: armed while the GPU is live, naming the attempted
        // backend. A stale file at the next launch means this run aborted, so
        // the resolver skips those backends instead of crashing again.
        // Inactive on macOS/wasm, where the resolver leaves the env alone.
        if OpenCADStudio::gpu_backend::gpu_guard_active() {
            OpenCADStudio::gpu_backend::arm_sentinel(
                gpu.backend_value.as_deref().unwrap_or("auto"),
            );
            // The backend is only a suspect while it is young: a run that
            // keeps drawing past this mark has proved it works, so a later
            // end — Task Manager, an OOM kill, a power cut — must not cost
            // the user that backend at the next launch.
            std::thread::spawn(|| {
                std::thread::sleep(OpenCADStudio::gpu_backend::SENTINEL_PROOF_DELAY);
                OpenCADStudio::gpu_backend::mark_sentinel_survived();
            });
            let result = app::run();
            OpenCADStudio::gpu_backend::disarm_sentinel();
            result
        } else {
            app::run()
        }
    }
}
