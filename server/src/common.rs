/**
 * THE LOST EXPEDITION - common.rs
 *
 * Shared data structures and gameplay constants.
 * Extended from the Vibe Coding Starter Pack with EXPEDITION tuning values.
 */

use spacetimedb::SpacetimeType;

// --- Shared Structs ---

// Helper struct for 3D vectors
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct Vector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

// Helper struct for player input state
#[derive(SpacetimeType, Clone, Debug)]
pub struct InputState {
    pub forward: bool,
    pub backward: bool,
    pub left: bool,
    pub right: bool,
    pub sprint: bool,
    pub jump: bool,
    pub attack: bool,
    pub cast_spell: bool,
    pub sequence: u32,
}

// --- Movement Constants ---
pub const PLAYER_SPEED: f32 = 9.0;          // fast traversal for the MASSIVE world
pub const SPRINT_MULTIPLIER: f32 = 1.8;

// --- World bounds ---
// KEEP: player_logic.rs references crate::common::GATE_RADIUS for the arena clamp
// (limit = GATE_RADIUS + 25.0). Stripping it breaks movement. Repurposed here as the
// expedition play-field radius now that the heist gates are gone.
pub const GATE_RADIUS: f32 = 40.0;          // expedition play-field radius

// --- NPC / interaction tuning ---
// Per-NPC mutual-exclusion + stale-claim window, in ticks (1Hz clock). 60 ticks is well
// above LLM p99; the renew_claim heartbeat keeps a slow-but-alive director from being reclaimed.
pub const CLAIM_TIMEOUT_TICKS: i64 = 60;
// Minimum ticks between accepted utterances to one NPC from any single asker (anti-spam).
pub const MIN_GAP: i64 = 2;
// Max chars retained from a player's STT transcript on the private npc_utterance row.
pub const TRANSCRIPT_CAP: usize = 400;
// Max chars of NPC dialogue rendered to clients (subtitle + TTS source).
pub const DIALOGUE_CAP: usize = 300;
// npc_interaction prune cap: never exceed this many done/failed rows; prune oldest first.
pub const INTERACTION_PRUNE_CAP: usize = 60;
// Max rows pruned from npc_interaction per tick (bounds tick work).
pub const INTERACTION_PRUNE_BATCH: usize = 20;
