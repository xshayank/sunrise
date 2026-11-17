//! A client mod's writes to the player it moves itself (`ClientHost::set_local_player_velocity`
//! and `set_local_player_on_ground`): what a prediction such as a belt's push or a boots'
//! hover sets before the tick's own move. Nothing is sent; the server hears of the result in
//! the position report the move produces and checks it as it checks any report.
use super::Game;
use sunrise_core::geometry::Vec3;

impl Game<'_> {
    /// `Entity.setDeltaMovement` on the local player. `false`, with the velocity unchanged,
    /// with no player of its own and for a value that is not finite.
    pub(super) fn set_local_player_velocity(&mut self, velocity: [f64; 3]) -> bool {
        let velocity = Vec3::new(velocity[0], velocity[1], velocity[2]);
        match self.player.as_mut() {
            Some(body) if velocity.is_finite() => {
                body.velocity = velocity;
                true
            }
            _ => false,
        }
    }

    /// The local player's on-ground flag, which this tick's jump decision reads and its move
    /// then sets again from the collision. `false` with no player of its own.
    pub(super) fn set_local_player_on_ground(&mut self, on_ground: bool) -> bool {
        match self.player.as_mut() {
            Some(body) => {
                body.ground.on_ground = on_ground;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::game::Game;
    use crate::game::native_entity_refs::tests::with_game;
    use mod_api::HostSide;
    use std::sync::{Arc, Mutex};

    /// Writes a velocity and a ground flag before the local player's tick, and records what
    /// the tick left of them.
    struct Boots {
        velocity: [f64; 3],
        saw: Arc<Mutex<Vec<(Vec<bool>, [f64; 3])>>>,
    }
    #[mod_api::__subscriptions]
    impl mod_api::Hooks for Boots {
        fn player_tick_pre(&self, host: HostSide<'_>, event: &mut mod_api::PlayerTickPreEvent) {
            let HostSide::Client(host) = host else {
                return;
            };
            if host.local_player() != Some(event.player) {
                return;
            }
            let wrote = vec![
                host.set_local_player_velocity(self.velocity),
                host.set_local_player_on_ground(true),
                host.set_local_player_velocity([f64::NAN, 0.0, 0.0]),
            ];
            self.saw.lock().unwrap().push((wrote, self.velocity));
        }
    }

    /// A client of a server whose local player is loaded, in the air over an empty chunk.
    fn loaded_client(game: &mut Game<'_>) {
        game.options.execution_role = crate::game::execution_role::ExecutionRole::Client;
        let (client, _server) = sunrise_protocol::connection::Pipe::pair();
        game.connect_local(client, "writes").unwrap();
        let remote = game.remote.as_mut().unwrap();
        remote.logged_in = true;
        remote.profile = 456;
        remote.entity_id = 42;
        remote.loaded_sent = true;
        game.player = sunrise_client::motion::PlayerMotion::new(
            game.motion,
            sunrise_core::geometry::Vec3::new(0.5, 64.0, 0.5),
        )
        .ok();
        game.streaming
            .insert_remote(&mut game.world, sunrise_client::chunk::Chunk::empty(0, 0));
    }

    #[test]
    #[ignore = "daily"]
    fn a_velocity_written_before_the_tick_is_what_the_tick_moves_by() {
        let saw = Arc::new(Mutex::new(Vec::new()));
        let mods = vec![mod_api::ModEntry::new(
            "boots",
            Box::new(Boots {
                velocity: [0.25, 0.0, 0.0],
                saw: Arc::clone(&saw),
            }),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            with_game(|game| {
                loaded_client(game);
                let before = game.player.as_ref().unwrap().position;
                game.tick_remote().unwrap();
                let body = game.player.as_ref().unwrap();
                let heard = saw.lock().unwrap();
                assert_eq!(heard.len(), 1, "the hook fires once a tick");
                assert_eq!(
                    heard[0].0,
                    [true, true, false],
                    "a finite velocity and the flag are written, a NaN is refused"
                );
                let moved = body.position.x - before.x;
                assert!((moved - 0.25).abs() < 1.0e-9, "moved {moved}");
                assert!(!body.ground.on_ground, "the move set the flag from the air");
            });
        });
    }

    #[test]
    #[ignore = "daily"]
    fn with_no_player_of_its_own_both_writes_answer_false() {
        with_game(|game| {
            game.player = None;
            assert!(!game.set_local_player_velocity([0.0; 3]));
            assert!(!game.set_local_player_on_ground(true));
        });
    }
}
