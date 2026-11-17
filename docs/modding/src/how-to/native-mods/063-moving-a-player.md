<!-- chapter: Moving a player -->
<!-- separator: 1 -->

### Moving a player

`player.entity().move_by(host, delta, cause)` (`EntityRef::move_by`, the call that moves any other entity, on a
`&mut dyn ServerHost`) moves a player by `delta` through the level as `Entity.move` would: blocks, other entities' colliders and the player's
collision exclusions stop it, and a step up is taken. It returns the displacement made. As for every entity, a
`delta` beyond 64 blocks on an axis is refused. The `entity_move` hook hears it once, for
the player and with `cause`, and a listener may replace or cancel it as for any other entity (a cancelled move is
`Some([0.0; 3])` and the client is told nothing); `cause` changes only what the hook hears, not the physics.

The server's copy moves at once, and the player's client is sent a relative teleport: it adds the displacement to where
it is when the packet arrives and keeps its motion and look, so what it walked in the meantime is kept. Until it
confirms, which a client does at the start of its next frame, the server drops the position of the player's own reports
(their rotation is kept, and nothing is counted against the player).

So `move_by` is for a one-off move, a launch or a pull, and not for every tick: a development build logs a warning when
one player is moved on 20 ticks in a row. For motion on every tick, use `player.entity().push(host, delta)` from the
server half of the mod, or move the player from the client half of the mod.

A player on a moving collider stays on it when the target is still over it, and lets go otherwise. The call is refused
(`None`, nothing moves) for a value that is not finite, a target outside the world, a player that is riding something,
a sleeper, a player without a connection, or a move the collision query cannot answer.

#### The client half

The client moves its own player, so a prediction (a belt's push, boots that hover) is written there, before the tick's
own move, from the `player_tick_pre` hook (`HostSide::Client`):

- `host.set_local_player_velocity([x, y, z])` replaces the local player's velocity, in blocks per tick (`false`, and
  nothing changes, for a value that is not finite or with no player of its own).
- `host.set_local_player_on_ground(flag)` sets its on-ground flag, which this tick's jump decision reads and the move
  then sets again from the collision (`false` with no player of its own).

Nothing is sent by either. The server hears of the result in the position report the move produces and checks it as
it checks any report, so a velocity the speed test or the collision check refuses is corrected there. Neither is a
server method: the server refuses a write of a player's velocity or ground flag. A server's write of a player's motion
reaches that player's client only through an explicit motion packet, which replaces the client's velocity outright.

At `player_tick_pre` the player's input fields hold the previous tick's applied values, so to scale this tick's forward
input (Create's boots multiply a slow one by 1.3) use the `movement_input` hook, which runs after the keys are read
and before they are applied.
