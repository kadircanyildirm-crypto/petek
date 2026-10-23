//! Rollback networking, in miniature.
//!
//! Two players steer and shoot. The remote player's inputs arrive a few
//! frames late, so the local peer guesses them and simulates ahead. When a
//! guess turns out wrong, it restores the snapshot from before that frame,
//! and simulates again with the real input. In the end, its world must match,
//! checksum and all, the world of a peer that had every input on time.
//!
//! Run it with `cargo run --example rollback`.

use std::collections::VecDeque;

use petek::command::Commands;
use petek::query::Query;
use petek::schedule::Schedule;
use petek::system::Res;
use petek::{Component, Entity, Resource, Snapshot, World};

/// How many frames the remote player's inputs take to arrive.
const DELAY: u64 = 4;
/// How many frames the game runs.
const FRAMES: u64 = 600;

/// What a player does in a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Input {
    steer: i32,
    fire: bool,
}

// Fixed-point integers rather than floats, so that the simulation gives the
// same results on every platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Position(i32, i32);
impl Component for Position {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Velocity(i32, i32);
impl Component for Velocity {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Player(usize);
impl Component for Player {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Bullet {
    frames_left: u32,
}
impl Component for Bullet {}

/// This frame's input of each player.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Inputs([Input; 2]);
impl Resource for Inputs {}

fn steer(inputs: Res<Inputs>, players: Query<(&Player, &mut Velocity)>) {
    for (player, mut velocity) in players {
        velocity.0 = (velocity.0 + inputs.0[player.0].steer).clamp(-8, 8);
    }
}

fn fire(inputs: Res<Inputs>, players: Query<(&Player, &Position)>, mut commands: Commands) {
    for (player, position) in players {
        if inputs.0[player.0].fire {
            commands.spawn((*position, Velocity(0, 16), Bullet { frames_left: 90 }));
        }
    }
}

fn movement(query: Query<(&mut Position, &Velocity)>) {
    query.into_iter().for_each(|(mut position, velocity)| {
        position.0 = (position.0 + velocity.0).rem_euclid(1000);
        position.1 = (position.1 + velocity.1).rem_euclid(1000);
    });
}

fn expire(bullets: Query<(Entity, &mut Bullet)>, mut commands: Commands) {
    for (entity, mut bullet) in bullets {
        bullet.frames_left -= 1;
        if bullet.frames_left == 0 {
            commands.despawn(entity);
        }
    }
}

/// A peer's copy of the game.
struct Game {
    world: World,
    schedule: Schedule,
}

impl Game {
    fn new() -> Self {
        let mut world = World::new();
        // Snapshots clone everything the world holds, and checksums hash
        // what peers compare. Registering every type at startup, in the same
        // order on every peer, also gives the types the same ids everywhere.
        world.register_clone::<Position>();
        world.register_clone::<Velocity>();
        world.register_clone::<Player>();
        world.register_clone::<Bullet>();
        world.register_resource_clone::<Inputs>();
        world.register_hash::<Position>();
        world.register_hash::<Velocity>();
        world.register_hash::<Player>();
        world.register_hash::<Bullet>();

        world.insert_resource(Inputs::default());
        world.spawn((Player(0), Position(250, 500), Velocity(0, 0)));
        world.spawn((Player(1), Position(750, 500), Velocity(0, 0)));

        let mut schedule = Schedule::new();
        schedule
            .add_system(steer)
            .add_system(fire)
            .add_system(movement)
            .add_system(expire);
        Self { world, schedule }
    }

    /// Simulates one frame with the given inputs.
    fn step(&mut self, frame: u64, remote: Input) {
        let inputs = Inputs([input_of(0, frame), remote]);
        self.world.insert_resource(inputs);
        self.schedule.run(&mut self.world);
    }
}

/// A frame simulated with a guess for the remote player's input.
struct Guessed {
    frame: u64,
    /// The world before the frame, to go back to if the guess was wrong.
    before: Snapshot,
    guess: Input,
}

/// The local peer, which knows the remote player's inputs `DELAY` frames
/// late.
struct Peer {
    game: Game,
    /// The frames whose remote input has not arrived yet, oldest first.
    guessed: VecDeque<Guessed>,
    /// The newest remote input that arrived, from which the next ones are
    /// guessed.
    newest: Input,
    rollbacks: u32,
    resimulated: u32,
}

impl Peer {
    fn new() -> Self {
        Self {
            game: Game::new(),
            guessed: VecDeque::new(),
            newest: Input::default(),
            rollbacks: 0,
            resimulated: 0,
        }
    }

    /// Simulates `frame`, guessing that the remote player keeps doing what
    /// they last did.
    fn advance(&mut self, frame: u64) {
        let guess = self.newest;
        let before = self.game.world.snapshot();
        self.guessed.push_back(Guessed {
            frame,
            before,
            guess,
        });
        self.game.step(frame, guess);
    }

    /// Takes the remote input of the oldest guessed frame, which has just
    /// arrived.
    fn confirm(&mut self, real: Input) {
        self.newest = real;
        let settled = self.guessed.pop_front().expect("a frame was guessed");
        if settled.guess == real {
            return;
        }
        // The guess was wrong: go back to before that frame, and simulate it
        // and every later frame again, guessing better.
        self.rollbacks += 1;
        self.game.world.restore(&settled.before);
        self.game.step(settled.frame, real);
        self.resimulated += 1;
        for later in &mut self.guessed {
            later.before = self.game.world.snapshot();
            later.guess = real;
            self.game.step(later.frame, real);
            self.resimulated += 1;
        }
    }
}

/// The input a player gives in a frame: made up, but the same on every run.
/// Players hold an input for a few frames, as people do.
fn input_of(player: usize, frame: u64) -> Input {
    let player = u64::try_from(player).expect("few players");
    let mut bits = (frame / 7) * 2 + player + 1;
    bits ^= bits << 13;
    bits ^= bits >> 7;
    bits ^= bits << 17;
    Input {
        steer: i32::try_from(bits % 3).expect("small") - 1,
        fire: bits % 4 == 0 && frame % 4 == 0,
    }
}

fn main() {
    let mut on_time = Game::new();
    for frame in 0..FRAMES {
        on_time.step(frame, input_of(1, frame));
    }

    let mut peer = Peer::new();
    for frame in 0..FRAMES {
        if let Some(arrived) = frame.checked_sub(DELAY) {
            peer.confirm(input_of(1, arrived));
        }
        peer.advance(frame);
    }
    for arrived in FRAMES.saturating_sub(DELAY)..FRAMES {
        peer.confirm(input_of(1, arrived));
    }

    let world = &peer.game.world;
    let bullets = |world: &World| world.query::<&Bullet>().iter().count();
    println!("frames simulated:        {FRAMES}, inputs {DELAY} frames late");
    println!("wrong guesses:           {}", peer.rollbacks);
    println!("frames simulated again:  {}", peer.resimulated);
    println!(
        "bullets in flight:       {} here, {} on time",
        bullets(world),
        bullets(&on_time.world),
    );
    println!(
        "checksums:               {:016x} here, {:016x} on time",
        world.checksum(),
        on_time.world.checksum(),
    );
    assert_eq!(world.checksum(), on_time.world.checksum());
    assert!(peer.rollbacks > 0, "some guesses should have been wrong");
    println!("the worlds match");
}
