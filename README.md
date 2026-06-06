# The Lost Expedition

A multiplayer **psychological-survival** game. You and your team are search-and-rescue, dropped
into a photorealistic old-growth forest to find a lost billionaire. The survivors and cultists you
find are **LLM-driven NPCs** with their own trauma, paranoia, and shifting allegiances — interrogate
them, but the *environment* shapes how they react, and trust can curdle into betrayal.

Built for the **SpacetimeDB Launchpad Hackathon**.

## Architecture

- **SpacetimeDB** (Rust module — `server/`) — server-authoritative game state + the NPC tables.
  The entire NPC interaction loop lives here; clients react to row changes via subscription.
- **Claude NPC Director** (Node — `keeper/`) — a privileged SpacetimeDB client and the *only*
  holder of the Anthropic key. It watches for player interactions, builds an environment-aware
  prompt, calls Claude for **structured JSON**, and writes the NPC's response + behavior back to
  the DB. No API key ever ships in the game client.
- **Unity client** (HDRP, photoreal forest) — *next phase* — renders the world + NPCs, captures
  voice (speech-to-text) → interaction, plays replies as subtitles + TTS, and drives animation +
  NavMesh from the NPC's replicated state.

## The NPC loop (server-authoritative, naturally async)

```
ask_npc(speech, environment)            # player speaks to an NPC
   -> director claims the interaction   # per-NPC lock; no double-processing
   -> Claude(persona + world context)   # structured JSON out
   -> npc_respond(...)                  # writes dialogue/animation/action/trust/sanity
   -> every client syncs the new state  # subscription, no RPC
```

The LLM must return strict JSON:

```json
{
  "dialogue": "...",
  "animation_trigger": "Cower | Threaten | Nod | Panic | Idle",
  "game_action": "FLEE | FOLLOW | STAY_PUT | ATTACK",
  "trust_change":  -20..20,
  "sanity_change": -15..15
}
```

**The environment drives behavior.** Approach unarmed and gentle and a survivor will cower and
slowly trust you; draw a weapon and put a flashlight in a cultist's face and trust collapses and he
turns to threaten you — same engine, opposite outcome.

## Status

- ✅ **SpacetimeDB NPC backend** (schema + reducers) — built and verified end-to-end through Claude.
- ✅ **Node NPC director** — environment-aware, structured output, trust/sanity clamping,
  single-flight per NPC with heartbeat-renewed claims.
- ⏳ **Unity client** (NPC agents, voice STT, subtitles + TTS, NavMesh) — next.

## Running the backend (local)

```bash
# 1) SpacetimeDB module
cd server && spacetime build && spacetime publish --server local vibe-multiplayer --delete-data -y

# 2) Claude NPC director  (set ANTHROPIC_API_KEY in keeper/.env first — see keeper/.env.example)
cd keeper && npm install && npm start
```
