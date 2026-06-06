/**
 * THE LOST EXPEDITION - lib.rs
 *
 * SpacetimeDB module: a co-op psychological survival game. Players are a
 * search-and-rescue team in a photoreal old-growth forest hunting a lost
 * billionaire. The NPCs (survivors / cultists) are LLM-driven via an external
 * privileged Node "director" process that reads pending interactions from the
 * DB and writes structured results back. WASM reducers CANNOT make outbound
 * HTTP — this module is a pure server-authoritative in/out mailbox.
 *
 * Shared state (all server-authoritative):
 *   player          - the rescue team: position, movement, status (core sync)
 *   npc             - PUBLIC fat row clients render: one per LLM-driven NPC
 *                     (dialogue / animation / action / target / trust / sanity
 *                     + per-NPC mutual-exclusion lock). NO secrets.
 *   npc_cognition   - PRIVATE: persona seed + rolling memory + last raw output
 *   npc_interaction - PUBLIC append-only player->director mailbox + per-row CAS
 *   npc_utterance   - PRIVATE: raw STT transcript (not broadcast)
 *   world_state     - PUBLIC singleton: the now_tick clock + global env inputs
 *
 * Time model: a monotonic `now_tick` (seconds) advanced by the 1Hz scheduled
 * game_tick, stored on world_state(0). All tick fields reference it.
 */

mod common;
mod player_logic;

use spacetimedb::{ReducerContext, Identity, Table, ScheduleAt, Timestamp};
use std::time::Duration;

use crate::common::*;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[spacetimedb::table(accessor = player, public)]
#[derive(Clone)]
pub struct PlayerData {
    #[primary_key]
    identity: Identity,
    username: String,
    character_class: String,
    position: Vector3,
    rotation: Vector3,
    health: i32,
    max_health: i32,
    mana: i32,
    max_mana: i32,
    current_animation: String,
    is_moving: bool,
    is_running: bool,
    is_attacking: bool,
    is_casting: bool,
    last_input_seq: u32,
    input: InputState,
    color: String,
}

#[spacetimedb::table(accessor = logged_out_player)]
#[derive(Clone)]
pub struct LoggedOutPlayerData {
    #[primary_key]
    identity: Identity,
    username: String,
    character_class: String,
    color: String,
    last_seen: Timestamp,
}

// PUBLIC. The single fat row clients render — one per LLM-driven survivor/cultist.
// Holds ONLY player-facing structured output + replicated trust/sanity + the
// per-NPC mutual-exclusion lock. NO prompts/secrets/transcripts (public tables
// sync whole rows). Clients subscribe to npc and edge-trigger on `seq`.
#[spacetimedb::table(accessor = npc, public)]
#[derive(Clone)]
pub struct Npc {
    #[primary_key]
    #[auto_inc]
    npc_id: u64,
    display_name: String,           // subtitle attribution + TTS voice selection
    archetype: String,              // survivor | cultist (public: director keys persona map on it)
    position: Vector3,              // spawn/teleport seed + coarse sync ONLY (client derives NavMesh dest)
    dialogue: String,               // subtitle text + TTS source (capped 300 in npc_respond)
    animation_trigger: String,      // Cower|Threaten|Nod|Panic|Idle -> animator.SetTrigger
    game_action: String,            // FLEE|FOLLOW|STAY_PUT|ATTACK -> NavMeshAgent behavior (EXACTLY 4)
    target_player: Option<Identity>,// concrete ref for FLEE/FOLLOW/ATTACK (NO index: Option not FilterableValue)
    trust: i32,                     // server-clamped 0..=100
    sanity: i32,                    // server-clamped 0..=100
    seq: u32,                       // monotonic edge-trigger; bumped once per successful npc_respond
    last_spoke_tick: i64,           // staleness / rate-limit stamp
    busy_until_tick: i64,           // PER-NPC LOCK window (authoritative)
    busy_interaction_id: u64,       // interaction that owns the busy window (0 = free)
}

// PRIVATE (omit `public`). Authoring-only persona seed + rolling memory + last
// raw output. Reducer-write only. The director's PRIMARY persona path is
// in-process keyed by npc_id, built from the PUBLIC npc.archetype; this table
// persists authored personas across module restarts (optional owner-token load).
#[spacetimedb::table(accessor = npc_cognition)]
#[derive(Clone)]
pub struct NpcCognition {
    #[primary_key]
    npc_id: u64,                    // 1:1 with npc; NOT auto_inc (seeded with the npc's assigned id)
    persona: String,                // full per-NPC persona seed (secret)
    memory: String,                 // rolling capped summary of past exchanges
    last_raw_output: String,        // last tool_use JSON for debugging (never synced)
}

// PUBLIC append-only inbox = player->director mailbox AND the per-row
// anti-double-processing CAS. ONE row per voice utterance. The raw STT
// transcript is NOT here (see npc_utterance) so interrogations aren't broadcast.
#[spacetimedb::table(accessor = npc_interaction, public)]
#[derive(Clone)]
pub struct NpcInteraction {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[index(btree)]
    npc_id: u64,                    // u64 IS FilterableValue
    #[index(btree)]
    asker: Identity,                // ctx.sender() of the speaker (from sender, not an arg — unforgeable)
    #[index(btree)]
    status: String,                 // pending | claimed | done | failed (the CAS guard column)
    claimed_by: Option<Identity>,   // director identity that won the CAS (None until claimed)
    player_distance: f32,           // env snapshot at speak-time
    is_player_armed: bool,          // env snapshot at speak-time
    flashlight_in_face: bool,       // env snapshot at speak-time
    transcript: String,             // the player's (STT) speech — PUBLIC so the anonymous director can read it
    trust_at_ask: i32,              // snapshot of npc.trust at ask-time (audit)
    sanity_at_ask: i32,             // snapshot of npc.sanity at ask-time (audit)
    created_tick: i64,              // FIFO ordering + stale sweep + client timeout cue
    claimed_tick: i64,              // set on claim, advanced by renew_claim heartbeat
}

// PRIVATE companion to npc_interaction holding the raw STT transcript so
// interrogation text is NOT broadcast on a public whole-row sync. Reducer-write
// (ask_npc) only. The director reads it via the owner-token path.
#[spacetimedb::table(accessor = npc_utterance)]
#[derive(Clone)]
pub struct NpcUtterance {
    #[primary_key]
    interaction_id: u64,            // 1:1 with npc_interaction.id
    transcript: String,             // STT result, capped 400 in ask_npc
}

// PUBLIC singleton (id:0). Absorbs now_tick relocated off the deleted Game table —
// without this, cur_tick() and every tick-based check break. Seeded in init BEFORE
// the first tick can fire; game_tick SELF-HEALS by inserting it if missing.
#[spacetimedb::table(accessor = world_state, public)]
#[derive(Clone)]
pub struct WorldState {
    #[primary_key]
    id: u32,                        // singleton, always 0
    now_tick: i64,                  // monotonic 1Hz clock (+1 each game_tick)
    time_of_day: String,            // dawn | day | dusk | night
    weather_conditions: String,     // clear | fog | rain | storm
}

#[spacetimedb::table(accessor = game_tick_schedule, public, scheduled(game_tick))]
pub struct GameTickSchedule {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) -> Result<(), String> {
    spacetimedb::log::info!("[INIT] THE LOST EXPEDITION module booting...");

    // (1) Seed world_state FIRST — before the schedule row — so cur_tick() is
    //     valid before any tick can fire.
    if ctx.db.world_state().id().find(0u32).is_none() {
        ctx.db.world_state().insert(WorldState {
            id: 0,
            now_tick: 0,
            time_of_day: "dusk".to_string(),
            weather_conditions: "fog".to_string(),
        });
    }

    // (2) KEPT VERBATIM: the 1Hz scheduled-reducer infra (count()==0 guard).
    if ctx.db.game_tick_schedule().count() == 0 {
        let schedule = GameTickSchedule {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(Duration::from_secs(1).into()),
        };
        let _ = ctx.db.game_tick_schedule().try_insert(schedule);
    }

    // (4) Seed the forest roster.
    seed_npcs(ctx);

    Ok(())
}

#[spacetimedb::reducer(client_connected)]
pub fn identity_connected(ctx: &ReducerContext) {
    spacetimedb::log::info!("Client connected: {}", ctx.sender());
}

#[spacetimedb::reducer(client_disconnected)]
pub fn identity_disconnected(ctx: &ReducerContext) {
    let id = ctx.sender();
    // NPCs persist regardless of client connections; only the player row is
    // snapshotted + removed on disconnect.
    if let Some(player) = ctx.db.player().identity().find(id) {
        ctx.db.logged_out_player().insert(LoggedOutPlayerData {
            identity: player.identity,
            username: player.username.clone(),
            character_class: player.character_class.clone(),
            color: player.color.clone(),
            last_seen: ctx.timestamp,
        });
        ctx.db.player().identity().delete(id);
    }
}

// ---------------------------------------------------------------------------
// Join / movement
// ---------------------------------------------------------------------------

#[spacetimedb::reducer]
pub fn register_player(ctx: &ReducerContext, username: String, character_class: String) {
    let id = ctx.sender();
    if ctx.db.player().identity().find(id).is_some() {
        return;
    }

    // Color cycles through a fixed palette by join order.
    let slot = ctx.db.player().count() as usize;
    let colors = ["cyan", "magenta", "yellow", "lightgreen", "orange", "white"];
    let color = colors[slot % colors.len()].to_string();

    // Clear any prior logged-out snapshot for this identity.
    if ctx.db.logged_out_player().identity().find(id).is_some() {
        ctx.db.logged_out_player().identity().delete(id);
    }

    // Spawn at the fixed expedition trailhead.
    let spawn = Vector3 { x: 0.0, y: 1.0, z: 0.0 };

    ctx.db.player().insert(PlayerData {
        identity: id,
        username,
        character_class,
        position: spawn,
        rotation: Vector3 { x: 0.0, y: 0.0, z: 0.0 },
        health: 100,
        max_health: 100,
        mana: 100,
        max_mana: 100,
        current_animation: "idle".to_string(),
        is_moving: false,
        is_running: false,
        is_attacking: false,
        is_casting: false,
        last_input_seq: 0,
        input: default_input(),
        color,
    });
}

#[spacetimedb::reducer]
pub fn update_player_input(
    ctx: &ReducerContext,
    input: InputState,
    _client_pos: Vector3,
    client_rot: Vector3,
    client_animation: String,
) {
    if let Some(mut player) = ctx.db.player().identity().find(ctx.sender()) {
        // No heist speed/stun coupling anymore: full speed, never stunned.
        player_logic::update_input_state(&mut player, input, client_rot, client_animation, 1.0, false);
        ctx.db.player().identity().update(player);
    }
}

// ---------------------------------------------------------------------------
// NPC interaction reducers
// ---------------------------------------------------------------------------

// PLAYER-FACING. The voice/STT entry point. Inserts a pending interaction row
// (the mailbox) + the private transcript companion, then returns the assigned
// interaction id so the client can correlate the eventual seq bump. Does NOT
// call the LLM (WASM can't HTTP) and does NOT mutate dialogue/trust/target.
#[spacetimedb::reducer]
pub fn ask_npc(
    ctx: &ReducerContext,
    npc_id: u64,
    transcript: String,
    player_distance: f32,
    is_player_armed: bool,
    flashlight_in_face: bool,
) -> Result<(), String> {   // reducers cannot return values; client correlates via its own npc_interaction row
    let asker = ctx.sender();
    let now = cur_tick(ctx);

    // (1) Validate the NPC exists.
    let Some(npc) = ctx.db.npc().npc_id().find(npc_id) else {
        return Err("npc not found".to_string());
    };

    // (2) RATE-LIMIT / DEDUP: reject if this asker already has a pending|claimed
    //     interaction for this NPC, or if the NPC spoke too recently.
    let has_open = ctx
        .db
        .npc_interaction()
        .npc_id()
        .filter(npc_id)
        .any(|i| i.asker == asker && (i.status == "pending" || i.status == "claimed"));
    if has_open {
        return Err("you already have a pending question for this npc".to_string());
    }
    if now - npc.last_spoke_tick < MIN_GAP {
        return Err("npc just spoke — wait a moment".to_string());
    }

    // (3) Cap transcript.
    let transcript: String = transcript.chars().take(TRANSCRIPT_CAP).collect();

    // (4) Snapshot live trust/sanity for audit.
    let trust_at_ask = npc.trust;
    let sanity_at_ask = npc.sanity;

    // (5) Insert the mailbox row; read back the assigned id.
    let row = ctx.db.npc_interaction().insert(NpcInteraction {
        id: 0,
        npc_id,
        asker,
        status: "pending".to_string(),
        claimed_by: None,
        player_distance,
        is_player_armed,
        flashlight_in_face,
        transcript: transcript.clone(),
        trust_at_ask,
        sanity_at_ask,
        created_tick: now,
        claimed_tick: 0,
    });
    let interaction_id = row.id;

    // (6) Insert the private transcript companion.
    ctx.db.npc_utterance().insert(NpcUtterance {
        interaction_id,
        transcript,
    });

    // (7) Do NOT write npc.target_player here — resolved only in npc_respond.
    let _ = interaction_id; // (kept for the npc_utterance link above; reducer returns unit)
    Ok(())
}

// Director only. PER-ROW CAS + PER-NPC LOCK. STDB serializes each reducer as one
// transaction: two directors/retries racing one row -> exactly one wins.
#[spacetimedb::reducer]
pub fn claim_interaction(ctx: &ReducerContext, interaction_id: u64) -> Result<(), String> {
    let now = cur_tick(ctx);

    let Some(mut interaction) = ctx.db.npc_interaction().id().find(interaction_id) else {
        return Err("interaction not found".to_string());
    };
    if interaction.status != "pending" {
        return Err("interaction not pending (lost race / already handled)".to_string());
    }

    // PER-NPC MUTEX: the real two-players-one-NPC serialization.
    let Some(mut npc) = ctx.db.npc().npc_id().find(interaction.npc_id) else {
        return Err("npc not found".to_string());
    };
    if npc.busy_until_tick > now && npc.busy_interaction_id != interaction_id {
        return Err("npc busy".to_string());
    }

    // Win: claim the row + lock the NPC.
    interaction.status = "claimed".to_string();
    interaction.claimed_by = Some(ctx.sender());
    interaction.claimed_tick = now;
    ctx.db.npc_interaction().id().update(interaction);

    npc.busy_until_tick = now + CLAIM_TIMEOUT_TICKS;
    npc.busy_interaction_id = interaction_id;
    ctx.db.npc().npc_id().update(npc);

    Ok(())
}

// Director only. Heartbeat while an LLM call is in flight: advances claimed_tick
// (and the NPC busy window) so game_tick's stale sweep doesn't reclaim a slow
// but alive call. Director calls it every ~10-20s while awaiting Claude.
#[spacetimedb::reducer]
pub fn renew_claim(ctx: &ReducerContext, interaction_id: u64) -> Result<(), String> {
    let now = cur_tick(ctx);

    let Some(mut interaction) = ctx.db.npc_interaction().id().find(interaction_id) else {
        return Err("interaction not found".to_string());
    };
    if interaction.status != "claimed" || interaction.claimed_by != Some(ctx.sender()) {
        return Err("not your active claim".to_string());
    }

    let npc_id = interaction.npc_id;
    interaction.claimed_tick = now;
    ctx.db.npc_interaction().id().update(interaction);

    if let Some(mut npc) = ctx.db.npc().npc_id().find(npc_id) {
        if npc.busy_interaction_id == interaction_id {
            npc.busy_until_tick = now + CLAIM_TIMEOUT_TICKS;
            ctx.db.npc().npc_id().update(npc);
        }
    }

    Ok(())
}

// Director only. The write-back, in ONE transaction. STRICT GUARD: the row must
// be claimed by the caller (closes the reclaim double-apply race). Trust/sanity
// are clamped server-side on the LIVE row (deltas never trusted); enums are
// validated with safe fallbacks; target_player resolved HERE atomic with seq.
#[spacetimedb::reducer]
pub fn npc_respond(
    ctx: &ReducerContext,
    npc_id: u64,
    interaction_id: u64,
    dialogue: String,
    animation_trigger: String,
    game_action: String,
    target_player: Option<Identity>,
    trust_change: i32,
    sanity_change: i32,
) -> Result<(), String> {
    let now = cur_tick(ctx);

    // STRICT GUARD.
    let Some(mut interaction) = ctx.db.npc_interaction().id().find(interaction_id) else {
        return Err("interaction not found".to_string());
    };
    if interaction.status != "claimed" || interaction.claimed_by != Some(ctx.sender()) {
        return Err("not your active claim".to_string());
    }

    let Some(mut npc) = ctx.db.npc().npc_id().find(npc_id) else {
        return Err("npc not found".to_string());
    };

    // Validate the closed enums; fall back so a client never gets garbage.
    let animation_trigger = match animation_trigger.as_str() {
        "Cower" | "Threaten" | "Nod" | "Panic" | "Idle" => animation_trigger,
        _ => "Idle".to_string(),
    };
    let game_action = match game_action.as_str() {
        "FLEE" | "FOLLOW" | "STAY_PUT" | "ATTACK" => game_action,
        _ => "STAY_PUT".to_string(),
    };

    // Clamp the ABSOLUTE result on the LIVE row (not the delta).
    npc.trust = (npc.trust + trust_change).clamp(0, 100);
    npc.sanity = (npc.sanity + sanity_change).clamp(0, 100);

    npc.dialogue = dialogue.chars().take(DIALOGUE_CAP).collect();
    npc.animation_trigger = animation_trigger;
    npc.game_action = game_action;
    npc.target_player = target_player; // resolved HERE, atomic with seq
    npc.seq += 1;
    npc.last_spoke_tick = now;

    // OWNERSHIP-SCOPED busy clear: only free the window THIS interaction owns.
    if npc.busy_interaction_id == interaction_id {
        npc.busy_until_tick = 0;
        npc.busy_interaction_id = 0;
    }
    ctx.db.npc().npc_id().update(npc);

    interaction.status = "done".to_string();
    ctx.db.npc_interaction().id().update(interaction);

    Ok(())
}

// Director only. Graceful-degrade last resort (the normal path uses npc_respond
// even for the rule-based fallback line so an NPC never freezes). Marks the row
// failed + ownership-clears the busy window.
#[spacetimedb::reducer]
pub fn fail_interaction(ctx: &ReducerContext, interaction_id: u64, reason: String) -> Result<(), String> {
    let Some(mut interaction) = ctx.db.npc_interaction().id().find(interaction_id) else {
        return Err("interaction not found".to_string());
    };
    if interaction.status != "claimed" || interaction.claimed_by != Some(ctx.sender()) {
        return Err("not your active claim".to_string());
    }

    let npc_id = interaction.npc_id;
    interaction.status = "failed".to_string();
    ctx.db.npc_interaction().id().update(interaction);

    if let Some(mut npc) = ctx.db.npc().npc_id().find(npc_id) {
        if npc.busy_interaction_id == interaction_id {
            npc.busy_until_tick = 0;
            npc.busy_interaction_id = 0;
            ctx.db.npc().npc_id().update(npc);
        }
    }

    spacetimedb::log::info!("[fail_interaction] {} reason: {}", interaction_id, reason);
    Ok(())
}

// Director/admin. Setter for the slow global env on world_state(0). Read by the
// director into environment.timeOfDay / weatherConditions.
#[spacetimedb::reducer]
pub fn set_world_state(ctx: &ReducerContext, time_of_day: String, weather_conditions: String) {
    if let Some(mut ws) = ctx.db.world_state().id().find(0u32) {
        ws.time_of_day = time_of_day;
        ws.weather_conditions = weather_conditions;
        ctx.db.world_state().id().update(ws);
    }
}

// Director only. Writes a rolling one-line summary to the PRIVATE npc_cognition
// row (reducer write = full DB access; no owner-token needed for WRITES).
// Optional — the director may instead keep memory in-process keyed by npc_id.
#[spacetimedb::reducer]
pub fn update_npc_memory(ctx: &ReducerContext, npc_id: u64, memory: String) {
    if let Some(mut cog) = ctx.db.npc_cognition().npc_id().find(npc_id) {
        cog.memory = memory;
        ctx.db.npc_cognition().npc_id().update(cog);
    }
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

// Idempotent: returns early if the roster already exists. Spawns a small forest
// roster (flat-XZ + small-Y, the same convention as player) each paired with a
// PRIVATE npc_cognition row carrying the archetype-specific persona. The director
// does NOT depend on reading npc_cognition — it builds its in-process persona map
// from the PUBLIC npc.archetype.
#[spacetimedb::reducer]
pub fn seed_npcs(ctx: &ReducerContext) {
    if ctx.db.npc().count() > 0 {
        return;
    }

    let roster: [(&str, &str, Vector3, &str); 3] = [
        (
            "Mara",
            "survivor",
            Vector3 { x: 18.0, y: 1.0, z: -12.0 },
            "You are Mara, the lost expedition's medic. Traumatized by what happened to the team. \
             You distrust armed strangers and recoil from a light in your face. You know where the \
             billionaire's camp was, but you fear the cultists and won't reveal it until you trust \
             the player. You are exhausted, grieving, and guarded.",
        ),
        (
            "Eli",
            "survivor",
            Vector3 { x: -22.0, y: 1.0, z: 8.0 },
            "You are Eli, the expedition's guide. Paranoid, your sanity is fraying after days alone \
             in the forest. You will FOLLOW a player who earns your trust, but you PANIC at a \
             flashlight in your face. You jump at shadows and second-guess every choice.",
        ),
        (
            "Brother Vael",
            "cultist",
            Vector3 { x: 5.0, y: 1.0, z: 34.0 },
            "You are Brother Vael, a zealot of the forest cult. You hide the cult's role in the \
             expedition's disappearance. You THREATEN when trust is low and speak in veiled, \
             ominous scripture. Your allegiance shifts only under very high trust — and even then \
             you guard the cult's deepest secret.",
        ),
    ];

    for (display_name, archetype, position, persona) in roster {
        let inserted = ctx.db.npc().insert(Npc {
            npc_id: 0,
            display_name: display_name.to_string(),
            archetype: archetype.to_string(),
            position,
            dialogue: String::new(),
            animation_trigger: "Idle".to_string(),
            game_action: "STAY_PUT".to_string(),
            target_player: None,
            trust: 50,
            sanity: 70,
            seq: 0,
            last_spoke_tick: 0,
            busy_until_tick: 0,
            busy_interaction_id: 0,
        });

        ctx.db.npc_cognition().insert(NpcCognition {
            npc_id: inserted.npc_id,
            persona: persona.to_string(),
            memory: String::new(),
            last_raw_output: String::new(),
        });
    }
}

// ---------------------------------------------------------------------------
// Scheduled tick (1Hz): advance clock + bounded DB housekeeping. NO HTTP.
// ---------------------------------------------------------------------------

#[spacetimedb::reducer(update)]
pub fn game_tick(ctx: &ReducerContext, _tick_info: GameTickSchedule) {
    // (1) SELF-HEAL the clock: never early-return on a missing row or a
    //     wiped/partial DB would freeze the clock forever.
    let Some(mut ws) = ctx.db.world_state().id().find(0u32) else {
        ctx.db.world_state().insert(WorldState {
            id: 0,
            now_tick: 1,
            time_of_day: "dusk".to_string(),
            weather_conditions: "fog".to_string(),
        });
        return;
    };
    ws.now_tick += 1;
    let now = ws.now_tick;
    ctx.db.world_state().id().update(ws);

    // (2) Stale-claim reclamation: iterate ONLY status=='claimed' rows (via the
    //     status btree index). Only a CRASHED (non-heartbeating) director is
    //     reclaimed thanks to the 60-tick window + renew_claim heartbeat.
    let stale: Vec<NpcInteraction> = ctx
        .db
        .npc_interaction()
        .status()
        .filter("claimed")
        .filter(|i| now - i.claimed_tick > CLAIM_TIMEOUT_TICKS)
        .collect();
    for mut interaction in stale {
        let iid = interaction.id;
        let npc_id = interaction.npc_id;
        interaction.status = "pending".to_string();
        interaction.claimed_by = None;
        ctx.db.npc_interaction().id().update(interaction);

        // Ownership-scoped: only clear the busy window THIS interaction owns.
        if let Some(mut npc) = ctx.db.npc().npc_id().find(npc_id) {
            if npc.busy_interaction_id == iid {
                npc.busy_until_tick = 0;
                npc.busy_interaction_id = 0;
                ctx.db.npc().npc_id().update(npc);
            }
        }
    }

    // (3) Expired-busy sweep: clear busy windows whose deadline has passed.
    let expired: Vec<u64> = ctx
        .db
        .npc()
        .iter()
        .filter(|n| n.busy_until_tick > 0 && now >= n.busy_until_tick)
        .map(|n| n.npc_id)
        .collect();
    for npc_id in expired {
        if let Some(mut npc) = ctx.db.npc().npc_id().find(npc_id) {
            npc.busy_until_tick = 0;
            npc.busy_interaction_id = 0;
            ctx.db.npc().npc_id().update(npc);
        }
    }

    // (4) Prune npc_interaction to a cap: delete the lowest-id done|failed rows
    //     ONLY (NEVER pending|claimed, regardless of age) + their paired
    //     transcript rows. Collect + sort in Rust (no ORDER BY in STDB).
    let count = ctx.db.npc_interaction().count() as usize;
    if count > INTERACTION_PRUNE_CAP {
        let mut finished: Vec<u64> = ctx
            .db
            .npc_interaction()
            .iter()
            .filter(|i| i.status == "done" || i.status == "failed")
            .map(|i| i.id)
            .collect();
        finished.sort_unstable();
        let over = count - INTERACTION_PRUNE_CAP;
        let to_delete = over.min(INTERACTION_PRUNE_BATCH).min(finished.len());
        for id in finished.into_iter().take(to_delete) {
            ctx.db.npc_interaction().id().delete(id);
            ctx.db.npc_utterance().interaction_id().delete(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_input() -> InputState {
    InputState {
        forward: false,
        backward: false,
        left: false,
        right: false,
        sprint: false,
        jump: false,
        attack: false,
        cast_spell: false,
        sequence: 0,
    }
}

// Squared planar distance — drives playerDistance verification + NPC range checks.
#[allow(dead_code)]
fn dist2(ax: f32, az: f32, bx: f32, bz: f32) -> f32 {
    let dx = ax - bx;
    let dz = az - bz;
    dx * dx + dz * dz
}

// The single time source: world_state(0).now_tick (relocated from the deleted Game table).
fn cur_tick(ctx: &ReducerContext) -> i64 {
    ctx.db.world_state().id().find(0u32).map(|w| w.now_tick).unwrap_or(0)
}
