//! Application shell: splash -> boot sequence (real model load) -> chat
//! with local Llama inference.

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use nr_model::{InferCtx, Model, Sampler};
use nr_runtime::{Arena, Clock};
use nr_token::Tokenizer;
use nr_ui::chat::{Role, Stats, Turn};
use nr_ui::loading::LoadState;
use nr_ui::Fonts;
use uefi::boot;
use uefi::boot::{AllocateType, MemoryType};

use crate::input::{self, InputEvent};
use crate::video::Display;

const CTX_LEN: usize = 4096;
const MAX_GEN_TOKENS: usize = 768;
/// Longest accepted prompt (chars). ~4 chars/token keeps this within
/// what the context window could ever hold.
const MAX_PROMPT_CHARS: usize = 8192;

fn stall_us(us: u64) {
    boot::stall(Duration::from_micros(us));
}

/// CPU architecture as spoken text (platform crate, so cfg is fine here).
const ARCH_NAME: &str = if cfg!(target_arch = "aarch64") {
    "ARM64"
} else {
    "x86_64"
};

fn system_prompt(model_name: &str) -> String {
    alloc::format!(
        "You are NightRun, a helpful assistant running {model_name} fully offline on bare-metal {ARCH_NAME} hardware - no operating system underneath. Be concise and friendly."
    )
}

pub struct Platform {
    pub display: Display,
    pub fonts: Fonts,
    pub clock: Clock,
    pub arena: Arena,
    pub ram_mb: u32,
    pub model: &'static Model<'static>,
    pub tokenizer: Tokenizer<'static>,
    pub model_name: String,
    pub infer: InferCtx<'static>,
    pub sampler: Sampler,
    pub cores: u32,
    pub pp_milli: u32,
    pub ftl_ms: u32,
    pub assistant: &'static str,
    #[cfg(feature = "network")]
    pub network: Option<crate::network::Network>,
}

pub fn run(display: Display) {
    let fonts = Fonts::load();
    let mut surf = nr_gfx::Surface::new(display.width, display.height);

    let footer = alloc::format!("v{}  //  press any key", crate::VERSION);
    nr_ui::splash::draw(&mut surf, &fonts, &footer);
    display.present(&surf);
    serial_println!("[app] splash");
    for _ in 0..350 {
        if input::poll().is_some() {
            break;
        }
        stall_us(10_000);
    }

    let clock = Clock::calibrate(stall_us);
    let mut platform = boot_sequence(display, fonts, clock, &mut surf);
    serial_println!(
        "[app] ready: model '{}' {} MB, ram {} MB",
        platform.model_name,
        platform.model.total_size() / (1024 * 1024),
        platform.ram_mb
    );

    chat_loop(&mut platform, &mut surf);
}

const STAGES: &[&str] = &[
    "initializing runtime",
    "scanning memory",
    "loading + verifying model",
    "preparing inference engine",
    "starting chat interface",
];

struct BootUi<'a> {
    display: &'a Display,
    surf: &'a mut nr_gfx::Surface,
    fonts: &'a Fonts,
    frame: u32,
}

impl BootUi<'_> {
    fn show(&mut self, current: usize, pm: u32, detail: &str) {
        let st = LoadState {
            stages: STAGES,
            current,
            progress_pm: pm,
            detail,
            frame: self.frame,
        };
        nr_ui::loading::draw(self.surf, self.fonts, &st);
        self.display.present(self.surf);
        self.frame += 1;
    }

    fn fail(&mut self, message: &str) -> ! {
        serial_println!("[boot] FATAL: {}", message);
        let st = LoadState {
            stages: STAGES,
            current: usize::MAX,
            progress_pm: 0,
            detail: message,
            frame: self.frame,
        };
        nr_ui::loading::draw(self.surf, self.fonts, &st);
        self.display.present(self.surf);
        loop {
            crate::halt();
        }
    }
}

fn boot_sequence(
    display: Display,
    fonts: Fonts,
    clock: Clock,
    surf: &mut nr_gfx::Surface,
) -> Platform {
    let t_boot = clock.now();
    let mut ui = BootUi {
        display: &display,
        surf,
        fonts: &fonts,
        frame: 0,
    };

    // Stage 0: runtime init (SIMD + multi-core bring-up).
    ui.show(0, 300, "TSC clock calibrated");
    let simd = nr_tensor::cpu::simd_label();
    ui.show(0, 600, simd);
    let (workers, smp_note) = crate::smp::start_workers();
    if workers == 0 {
        // Visible diagnosis on hardware without serial; linger briefly.
        ui.show(0, 1000, &alloc::format!("single core ({smp_note})"));
        stall_us(1_500_000);
    } else {
        ui.show(
            0,
            1000,
            &alloc::format!(
                "{} cores online ({} inference workers)",
                workers + 1,
                workers
            ),
        );
    }
    stall_us(150_000);

    // Pi 5: the fan is OS-managed and would otherwise stay off; run it
    // at 100% for the whole session (no thermal management exists here).
    #[cfg(target_arch = "aarch64")]
    {
        let fan = crate::fan::spin_up();
        ui.show(
            0,
            1000,
            if fan {
                "cooling fan: 100%"
            } else {
                "no fan control (RP1 not found)"
            },
        );
        stall_us(400_000);
    }

    // Stage 1: memory scan.
    let ram_mb = conventional_ram_mb();
    ui.show(1, 1000, &alloc::format!("{ram_mb} MB conventional RAM"));
    stall_us(120_000);

    // Stage 2: load model.nrm into RAM (chunked reads off the boot volume).
    let t0 = clock.now();
    let blob: &'static [u8] = {
        let ui = &mut ui;
        let clock = &clock;
        let mut cb = |done: usize, total: usize| {
            let pm = (done as u64 * 1000 / total.max(1) as u64) as u32;
            let mbs = {
                let ms = clock.ticks_to_ms(clock.now() - t0).max(1);
                done as u64 * 1000 / ms / (1024 * 1024)
            };
            ui.show(
                2,
                pm,
                &alloc::format!(
                    "{} / {} MB  ({} MB/s, CRC32 inline)",
                    done / (1024 * 1024),
                    total / (1024 * 1024),
                    mbs
                ),
            );
        };
        match crate::modelload::load(&mut cb) {
            Ok(buf) => buf,
            Err(crate::modelload::LoadError::Corrupt(which)) => ui.fail(&alloc::format!(
                "model.nrm is corrupt ({which:?} checksum mismatch) - re-flash the image or rebuild with: cargo xtask image"
            )),
            Err(e) => ui.fail(&alloc::format!(
                "model.nrm load failed ({e:?}) - build the image with: cargo xtask image"
            )),
        }
    };
    let load_ms = clock.ticks_to_ms(clock.now() - t0);
    serial_println!(
        "[boot] model loaded + verified (streaming CRC) in {} ms",
        load_ms
    );

    let model = match Model::parse(blob) {
        Ok(m) => m,
        Err(e) => ui.fail(&alloc::format!("model.nrm invalid: {e:?}")),
    };
    let tokenizer = match Tokenizer::parse(model.tokenizer_blob) {
        Ok(t) => t,
        Err(e) => ui.fail(&alloc::format!("tokenizer blob invalid: {e:?}")),
    };
    serial_println!(
        "[boot] vocab={} layers={}",
        tokenizer.vocab_len(),
        model.meta.n_layers
    );

    // Model is fully resident; nothing may touch storage from here on.
    crate::modelload::seal_storage();

    // Stage 4: arena + inference context, sized for this model's KV cache
    // and scratch at CTX_LEN (Llama 1B: ~140 MB, Qwen3 4B: ~640 MB).
    let need = InferCtx::required_bytes(&model, CTX_LEN) + 8 * 1024 * 1024;
    let arena_bytes = need.div_ceil(4096) * 4096;
    let pages = arena_bytes / 4096;
    let base = match boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages) {
        Ok(b) => b,
        Err(_) => ui.fail("arena allocation failed - machine needs more RAM"),
    };
    // SAFETY: freshly allocated, exclusively owned.
    let mut arena = unsafe { Arena::new(base.as_ptr(), arena_bytes) };
    let arena_mb = arena_bytes / (1024 * 1024);
    for off in (0..arena_bytes).step_by(16 * 1024 * 1024) {
        let len = (16 * 1024 * 1024).min(arena_bytes - off);
        unsafe { core::ptr::write_bytes(base.as_ptr().add(off), 0, len) };
        ui.show(
            3,
            ((off + len) * 500 / arena_bytes) as u32,
            &alloc::format!("{arena_mb} MB resident arena"),
        );
    }

    // The model must outlive the InferCtx that borrows it; both live for
    // the whole session.
    let model: &'static Model<'static> = alloc::boxed::Box::leak(alloc::boxed::Box::new(model));
    let mut alloc_cb = |bytes: usize, align: usize| -> *mut u8 {
        arena
            .alloc_bytes(bytes, align)
            .map(|s| s.as_mut_ptr())
            .unwrap_or(core::ptr::null_mut())
    };
    ui.show(
        3,
        750,
        &alloc::format!(
            "KV cache + scratch ({} MB, ctx {})",
            need / (1024 * 1024),
            CTX_LEN
        ),
    );
    let infer = match InferCtx::new(model, CTX_LEN, &mut alloc_cb) {
        Ok(i) => i,
        Err(e) => ui.fail(&alloc::format!("inference init failed: {e:?}")),
    };
    let sampler = Sampler::new(0.7, 0.9, nr_runtime::clock::rdtsc());
    ui.show(3, 1000, "inference engine ready");

    // Stage 5: done.
    let boot_ms = clock.ticks_to_ms(clock.now() - t_boot);
    serial_println!("[boot] chat-ready in {} ms (after splash)", boot_ms);
    ui.show(
        4,
        1000,
        &alloc::format!("boot sequence {}.{}s", boot_ms / 1000, boot_ms % 1000 / 100),
    );
    stall_us(400_000);

    // The converter writes the full display name incl. quant label.
    let model_name = alloc::format!("{}", model.meta.name_str());
    Platform {
        display,
        fonts,
        clock,
        arena,
        ram_mb,
        model,
        tokenizer,
        model_name,
        infer,
        sampler,
        cores: workers as u32 + 1,
        pp_milli: 0,
        ftl_ms: 0,
        assistant: model.meta.arch.assistant_label(),
        #[cfg(feature = "network")]
        network: crate::network::Network::init(),
    }
}

fn conventional_ram_mb() -> u32 {
    use uefi::mem::memory_map::MemoryMap as _;
    match boot::memory_map(MemoryType::LOADER_DATA) {
        Ok(map) => {
            let pages: u64 = map
                .entries()
                .filter(|d| d.ty == MemoryType::CONVENTIONAL)
                .map(|d| d.page_count)
                .sum();
            (pages * 4096 / (1024 * 1024)) as u32
        }
        Err(_) => 0,
    }
}

fn intro_turn(model_name: &str) -> Turn {
    #[cfg(feature = "mcp")]
    let network_help = " · /mcp-list and /mcp-call use the trusted gateway";
    #[cfg(not(feature = "mcp"))]
    let network_help = "";
    Turn {
        role: Role::System,
        text: alloc::format!(
            "{model_name} · resident in RAM · fully local, no OS underneath. Type a prompt;\nESC stops generation · UP/DOWN scroll history · /clear new conversation · /bye shut down{network_help}"
        ),
    }
}

fn chat_loop(p: &mut Platform, surf: &mut nr_gfx::Surface) {
    let mut turns: Vec<Turn> = Vec::new();
    turns.push(intro_turn(&p.model_name));
    let mut inputline = String::new();
    let mut caret: usize = 0; // char index into inputline
    let mut frame = 0u32;
    let mut last_rate = 0u32;
    let mut conversation_started = false;
    let mut scroll: usize = 0;
    let page = nr_ui::chat::page_lines(surf, &p.fonts);

    loop {
        #[cfg(feature = "mcp")]
        if let Some(rate) = service_mcp(
            p,
            surf,
            &mut turns,
            &mut conversation_started,
            frame,
            last_rate,
        ) {
            last_rate = rate;
        }
        #[cfg(all(feature = "network", not(feature = "mcp")))]
        if let Some(network) = p.network.as_mut() {
            network.poll();
        }
        let mut dirty = false;
        while let Some(ev) = input::poll() {
            dirty = true;
            match ev {
                InputEvent::Char(c) => {
                    // Bound the prompt: the context window can't use more
                    // anyway, and unbounded growth is a memory hazard.
                    if inputline.chars().count() < MAX_PROMPT_CHARS {
                        let b = byte_at(&inputline, caret);
                        inputline.insert(b, c);
                        caret += 1;
                    }
                }
                InputEvent::Backspace => {
                    if caret > 0 {
                        caret -= 1;
                        let b = byte_at(&inputline, caret);
                        inputline.remove(b);
                    }
                }
                InputEvent::Delete => {
                    if caret < inputline.chars().count() {
                        let b = byte_at(&inputline, caret);
                        inputline.remove(b);
                    }
                }
                InputEvent::Left => caret = caret.saturating_sub(1),
                InputEvent::Right => caret = (caret + 1).min(inputline.chars().count()),
                InputEvent::Enter => {
                    let prompt = core::mem::take(&mut inputline);
                    caret = 0;
                    let trimmed = prompt.trim();
                    scroll = 0;
                    match trimmed {
                        "" => {}
                        "/clear" => {
                            // Fresh conversation: transcript + KV position
                            // reset; the model stays resident in RAM.
                            turns.clear();
                            turns.push(intro_turn(&p.model_name));
                            turns.push(Turn {
                                role: Role::System,
                                text: String::from("New conversation started."),
                            });
                            p.infer.reset();
                            conversation_started = false;
                            p.pp_milli = 0;
                            p.ftl_ms = 0;
                            last_rate = 0;
                            serial_println!("[chat] /clear - conversation reset (model resident)");
                        }
                        "/bye" => {
                            turns.push(Turn {
                                role: Role::System,
                                text: String::from("Shutting down. Goodbye!"),
                            });
                            draw_chat(p, surf, &turns, "", 0, frame, 0, false, 0);
                            serial_println!("[chat] /bye - shutting down");
                            stall_us(800_000);
                            // Real UEFI power-off via runtime services.
                            uefi::runtime::reset(
                                uefi::runtime::ResetType::SHUTDOWN,
                                uefi::Status::SUCCESS,
                                None,
                            );
                        }
                        #[cfg(feature = "mcp")]
                        "/mcp-list" => {
                            queue_mcp_request(
                                p,
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": frame,
                                    "method": "tools/list",
                                    "params": {}
                                }),
                                &mut turns,
                            );
                        }
                        #[cfg(feature = "mcp")]
                        cmd if cmd.starts_with("/mcp-call ") => {
                            queue_mcp_call(p, cmd, frame, &mut turns);
                        }
                        cmd if cmd.starts_with('/') => {
                            turns.push(Turn {
                                role: Role::System,
                                text: alloc::format!(
                                    "unknown command {cmd} · /clear starts a new conversation · /bye shuts down"
                                ),
                            });
                        }
                        _ => {
                            serial_println!("[chat] user: {}", trimmed);
                            turns.push(Turn {
                                role: Role::User,
                                text: String::from(trimmed),
                            });
                            last_rate = generate(
                                p,
                                surf,
                                trimmed,
                                &mut turns,
                                &mut conversation_started,
                                frame,
                            );
                        }
                    }
                }
                InputEvent::Up => scroll = scroll.saturating_add(3),
                InputEvent::Down => scroll = scroll.saturating_sub(3),
                InputEvent::PageUp => scroll = scroll.saturating_add(page),
                InputEvent::PageDown => scroll = scroll.saturating_sub(page),
                _ => {}
            }
        }

        if dirty || frame % 8 == 0 {
            scroll = draw_chat(
                p, surf, &turns, &inputline, caret, frame, last_rate, false, scroll,
            );
        }
        frame = frame.wrapping_add(1);
        stall_us(16_000);
    }
}

#[cfg(feature = "mcp")]
fn service_mcp(
    p: &mut Platform,
    surf: &mut nr_gfx::Surface,
    turns: &mut Vec<Turn>,
    conversation_started: &mut bool,
    frame: u32,
    last_rate: u32,
) -> Option<u32> {
    let message = p.network.as_mut()?.poll_mcp()?;
    match message.kind {
        nr_mcp::Kind::HostRequest => {
            let (response, rate) =
                handle_inbound_mcp(p, surf, turns, conversation_started, frame, &message.json);
            if let Some(response) = response {
                if let Some(network) = p.network.as_mut() {
                    network.queue_response(message.message_id, response);
                }
            }
            Some(rate.unwrap_or(last_rate))
        }
        nr_mcp::Kind::HostResponse => {
            let text = serde_json::from_slice::<serde_json::Value>(&message.json)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .unwrap_or_else(|| String::from_utf8_lossy(&message.json).into_owned());
            turns.push(Turn {
                role: Role::System,
                text: alloc::format!("MCP response:\n{text}"),
            });
            Some(last_rate)
        }
        _ => Some(last_rate),
    }
}

#[cfg(feature = "mcp")]
fn handle_inbound_mcp(
    p: &mut Platform,
    surf: &mut nr_gfx::Surface,
    turns: &mut Vec<Turn>,
    conversation_started: &mut bool,
    frame: u32,
    bytes: &[u8],
) -> (Option<Vec<u8>>, Option<u32>) {
    use serde_json::{json, Value};

    let request: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => {
            return (
                Some(
                    serde_json::to_vec(&json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": "Parse error" }
                    }))
                    .unwrap(),
                ),
                None,
            );
        }
    };
    let Some(id) = request.get("id").cloned() else {
        // Notifications intentionally have no JSON-RPC response.
        return (None, None);
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "nightrun",
                "title": "NightRun bare-metal LLM",
                "version": crate::VERSION
            },
            "instructions": "Tools run on the local NightRun model. nightrun_prompt changes the visible conversation."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": [
                {
                    "name": "nightrun_status",
                    "title": "NightRun Status",
                    "description": "Report the resident model, memory, CPU cores, and context usage.",
                    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                },
                {
                    "name": "nightrun_prompt",
                    "title": "Prompt NightRun",
                    "description": "Send a prompt to the local bare-metal language model. This changes the visible conversation.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "prompt": { "type": "string", "minLength": 1, "maxLength": MAX_PROMPT_CHARS } },
                        "required": ["prompt"],
                        "additionalProperties": false
                    }
                }
            ]
        })),
        "tools/call" => {
            let params = request.get("params").unwrap_or(&Value::Null);
            match params.get("name").and_then(Value::as_str) {
                Some("nightrun_status") => Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": alloc::format!(
                            "model={} cores={} ram={}MB context={}/{}",
                            p.model_name,
                            p.cores,
                            p.ram_mb,
                            p.infer.pos,
                            p.infer.dims.ctx
                        )
                    }],
                    "isError": false
                })),
                Some("nightrun_prompt") => {
                    let prompt = params
                        .get("arguments")
                        .and_then(|args| args.get("prompt"))
                        .and_then(Value::as_str);
                    match prompt {
                        Some(prompt)
                            if !prompt.is_empty() && prompt.chars().count() <= MAX_PROMPT_CHARS =>
                        {
                            turns.push(Turn {
                                role: Role::User,
                                text: String::from(prompt),
                            });
                            let rate =
                                generate(p, surf, prompt, turns, conversation_started, frame);
                            let answer = turns
                                .last()
                                .map(|turn| turn.text.clone())
                                .unwrap_or_default();
                            return (
                                Some(
                                    serde_json::to_vec(&json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "content": [{ "type": "text", "text": answer }],
                                            "isError": false
                                        }
                                    }))
                                    .unwrap(),
                                ),
                                Some(rate),
                            );
                        }
                        _ => Err((-32602, "nightrun_prompt requires a non-empty prompt")),
                    }
                }
                Some(_) => Err((-32602, "Unknown tool")),
                None => Err((-32602, "tools/call requires a tool name")),
            }
        }
        _ => Err((-32601, "Method not found")),
    };
    let response = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    (Some(serde_json::to_vec(&response).unwrap()), None)
}

#[cfg(feature = "mcp")]
fn queue_mcp_request(p: &mut Platform, request: serde_json::Value, turns: &mut Vec<Turn>) {
    if let Some(network) = p.network.as_mut() {
        network.queue_upstream_request(serde_json::to_vec(&request).unwrap());
        turns.push(Turn {
            role: Role::System,
            text: String::from("MCP request queued through the trusted gateway."),
        });
    } else {
        turns.push(Turn {
            role: Role::System,
            text: String::from("MCP unavailable: no firmware network adapter."),
        });
    }
}

#[cfg(feature = "mcp")]
fn queue_mcp_call(p: &mut Platform, command: &str, id: u32, turns: &mut Vec<Turn>) {
    use serde_json::{json, Value};

    let mut parts = command.splitn(3, ' ');
    let _ = parts.next();
    let Some(name) = parts.next().filter(|name| !name.is_empty()) else {
        turns.push(Turn {
            role: Role::System,
            text: String::from("usage: /mcp-call TOOL {JSON arguments}"),
        });
        return;
    };
    let arguments = match parts.next() {
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => {
                turns.push(Turn {
                    role: Role::System,
                    text: String::from("MCP arguments must be one JSON object."),
                });
                return;
            }
        },
        None => json!({}),
    };
    queue_mcp_request(
        p,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
        turns,
    );
}

/// Run one user turn through the model, streaming tokens to the screen.
/// Returns milli-tokens/sec over the generation phase.
fn generate(
    p: &mut Platform,
    surf: &mut nr_gfx::Surface,
    prompt: &str,
    turns: &mut Vec<Turn>,
    conversation_started: &mut bool,
    mut frame: u32,
) -> u32 {
    // Out of context? Start a fresh conversation.
    if p.infer.remaining() < MAX_GEN_TOKENS + 256 {
        p.infer.reset();
        *conversation_started = false;
        turns.push(Turn {
            role: Role::System,
            text: String::from("context window full - conversation reset"),
        });
    }

    // Build this turn's token sequence (Llama-3 instruct template).
    let mut ids: Vec<u32> = Vec::new();
    if !*conversation_started {
        p.tokenizer
            .encode_conversation_start(Some(&system_prompt(&p.model_name)), &mut ids);
        *conversation_started = true;
    }
    p.tokenizer
        .encode_message(nr_token::template::ROLE_USER, prompt, &mut ids);
    p.tokenizer
        .encode_header(nr_token::template::ROLE_ASSISTANT, &mut ids);

    turns.push(Turn {
        role: Role::Llama,
        text: String::new(),
    });

    // Batched prefill. Chunks of 16 (not MAX_BATCH): each chunk boundary
    // is a redraw, which is what makes the thinking cursor blink while
    // the model has produced no text yet; pp cost vs 64 is negligible.
    let t0 = p.clock.now();
    for chunk in ids.chunks(16) {
        p.infer.prefill_chunk(chunk);
        draw_chat(p, surf, turns, "", 0, frame, 0, true, 0);
        frame = frame.wrapping_add(1);
        if matches!(input::poll(), Some(InputEvent::Escape)) {
            serial_println!("[gen] prefill interrupted");
            break;
        }
    }
    let prefill_ms = p.clock.ticks_to_ms(p.clock.now() - t0);
    p.pp_milli = (ids.len() as u64 * 1_000_000 / prefill_ms.max(1)) as u32;
    p.ftl_ms = prefill_ms as u32; // time until the first token can appear
    serial_println!(
        "[gen] prefill {} tokens in {} ms ({} tok/s)",
        ids.len(),
        prefill_ms,
        ids.len() as u64 * 1000 / prefill_ms.max(1)
    );

    // Generation loop.
    let t0 = p.clock.now();
    let mut produced = 0u64;
    let mut rate = 0u32;
    let mut utf8_pending: Vec<u8> = Vec::new();
    // The logits of the last prefilled token seed the first sample; re-run
    // sample/forward until a stop token, budget, or ESC.
    let mut next = {
        let logits = p.infer.logits();
        p.sampler.sample(logits)
    };
    while !p.tokenizer.is_stop(next) && produced < MAX_GEN_TOKENS as u64 && p.infer.remaining() > 0
    {
        utf8_pending.extend_from_slice(p.tokenizer.token_bytes(next));
        nr_token::flush_utf8(&mut utf8_pending, &mut turns.last_mut().unwrap().text);

        produced += 1;
        rate = p.clock.rate_milli(produced, p.clock.now() - t0);
        draw_chat(p, surf, turns, "", 0, frame, rate, true, 0);
        frame = frame.wrapping_add(1);

        if matches!(input::poll(), Some(InputEvent::Escape)) {
            serial_println!("[gen] interrupted by user");
            break;
        }

        let logits = p.infer.forward(next);
        next = p.sampler.sample(logits);
    }
    // Keep the template consistent for the next turn.
    if p.tokenizer.is_stop(next) && p.infer.remaining() > 0 {
        p.infer.forward(p.tokenizer.specials.eot);
    }

    let gen_ms = p.clock.ticks_to_ms(p.clock.now() - t0);
    serial_println!(
        "[gen] {} tokens in {} ms ({} milli-tok/s)",
        produced,
        gen_ms,
        rate
    );
    if turns.last().map(|t| t.text.is_empty()).unwrap_or(false) {
        turns.last_mut().unwrap().text = String::from("(no output)");
    }
    rate
}

/// Char index -> byte offset for caret edits (prompt text is UTF-8).
fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[allow(clippy::too_many_arguments)]
fn draw_chat(
    p: &Platform,
    surf: &mut nr_gfx::Surface,
    turns: &[Turn],
    input: &str,
    caret: usize,
    frame: u32,
    rate_milli: u32,
    generating: bool,
    scroll: usize,
) -> usize {
    let stats = Stats {
        model: &p.model_name,
        assistant: p.assistant,
        mem_used_mb: ((p.model.total_size() + p.arena.used()) / (1024 * 1024)) as u32,
        mem_total_mb: p.ram_mb,
        tok_s_milli: rate_milli,
        pp_milli: p.pp_milli,
        ftl_ms: p.ftl_ms,
        ctx_used: p.infer.pos as u32,
        ctx_max: p.infer.dims.ctx as u32,
        cores: p.cores,
        generating,
    };
    // Blink cadence: idle frames tick at ~60 Hz (divide down); generation
    // frames tick per prefill chunk / decoded token (toggle each redraw).
    let cursor_on = if generating {
        frame % 2 == 0
    } else {
        (frame / 16) % 2 == 0
    };
    let scroll = nr_ui::chat::draw(
        surf, &p.fonts, turns, input, caret, cursor_on, &stats, scroll,
    );
    p.display.present(surf);
    scroll
}
