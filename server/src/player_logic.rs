/**
 * TOMB RUSH - player_logic.rs
 *
 * Server-authoritative movement. Extended from the starter so that movement
 * respects a per-player speed multiplier (carrying loot / curses / blessings)
 * and a stun lock (after being shoved or hitting a trap).
 */

use crate::common::{Vector3, InputState, PLAYER_SPEED, SPRINT_MULTIPLIER};
use crate::PlayerData;

// Movement direction math (unchanged from the starter), now scaled by `speed_mult`.
pub fn calculate_new_position(
    position: &Vector3,
    rotation: &Vector3,
    input: &InputState,
    delta_time: f32,
    speed_mult: f32,
) -> Vector3 {
    let has_movement_input = input.forward || input.backward || input.left || input.right;
    if !has_movement_input {
        return position.clone();
    }

    let base = if input.sprint { PLAYER_SPEED * SPRINT_MULTIPLIER } else { PLAYER_SPEED };
    let speed = base * speed_mult;

    let yaw = rotation.y;
    let forward = Vector3 { x: 0.0, y: 0.0, z: -1.0 };
    let right = Vector3 { x: 1.0, y: 0.0, z: 0.0 };

    let cos_yaw = yaw.cos();
    let sin_yaw = yaw.sin();

    let rotated_forward = Vector3 {
        x: forward.x * cos_yaw + forward.z * sin_yaw,
        y: 0.0,
        z: -forward.x * sin_yaw + forward.z * cos_yaw,
    };
    let rotated_right = Vector3 {
        x: right.x * cos_yaw + right.z * sin_yaw,
        y: 0.0,
        z: -right.x * sin_yaw + right.z * cos_yaw,
    };

    let mut direction = Vector3 { x: 0.0, y: 0.0, z: 0.0 };
    if input.forward {
        direction.x -= rotated_forward.x;
        direction.z -= rotated_forward.z;
    }
    if input.backward {
        direction.x += rotated_forward.x;
        direction.z += rotated_forward.z;
    }
    if input.right {
        direction.x -= rotated_right.x;
        direction.z -= rotated_right.z;
    }
    if input.left {
        direction.x += rotated_right.x;
        direction.z += rotated_right.z;
    }

    let magnitude = (direction.x.powi(2) + direction.z.powi(2)).sqrt();
    if magnitude > 0.01 {
        direction.x /= magnitude;
        direction.z /= magnitude;
    }

    direction.x *= speed * delta_time;
    direction.z *= speed * delta_time;

    let mut new_position = position.clone();
    new_position.x += direction.x;
    new_position.z += direction.z;

    // Keep players inside the arena (gates ring at GATE_RADIUS; small buffer beyond).
    let limit = crate::common::GATE_RADIUS + 25.0;
    let dist = (new_position.x * new_position.x + new_position.z * new_position.z).sqrt();
    if dist > limit {
        new_position.x *= limit / dist;
        new_position.z *= limit / dist;
    }

    new_position
}

// Update player state based on input, respecting speed multiplier + stun.
pub fn update_input_state(
    player: &mut PlayerData,
    input: InputState,
    client_rot: Vector3,
    client_animation: String,
    speed_mult: f32,
    stunned: bool,
) {
    let delta_time_estimate: f32 = 1.0 / 20.0; // ~20Hz client send rate

    if !stunned {
        let new_position = calculate_new_position(
            &player.position,
            &client_rot,
            &input,
            delta_time_estimate,
            speed_mult,
        );
        player.position = new_position;
    }

    player.rotation = client_rot;
    player.current_animation = client_animation;
    player.input = input.clone();
    player.last_input_seq = input.sequence;
    player.is_moving = !stunned && (input.forward || input.backward || input.left || input.right);
    player.is_running = player.is_moving && input.sprint;
    player.is_attacking = input.attack;
    player.is_casting = input.cast_spell;
}
