//! Short monophonic game-theme arrangements for the GPIO7 passive piezo.
//!
//! These are intentionally compact motifs rather than complete soundtracks:
//! they fit the Leaf's one-voice square-wave output and make useful hardware
//! demos without turning the normal Leaf runtime into a music player.

#[derive(Clone, Copy)]
pub struct Note {
    pub frequency_hz: u32,
    pub duration_ms: u32,
}

#[derive(Clone, Copy)]
pub struct Tune {
    pub title: &'static str,
    pub notes: &'static [Note],
}

const fn n(frequency_hz: u32, duration_ms: u32) -> Note {
    Note {
        frequency_hz,
        duration_ms,
    }
}

const R: u32 = 0;
const DS7: u32 = 2489;
const E6: u32 = 1319;
const G6: u32 = 1568;
const GS6: u32 = 1661;
const A6: u32 = 1760;
const AS6: u32 = 1865;
const B6: u32 = 1976;
const C7: u32 = 2093;
const D7: u32 = 2349;
const E7: u32 = 2637;
const F7: u32 = 2794;
const FS7: u32 = 2960;
const G7: u32 = 3136;
const A7: u32 = 3520;
const B7: u32 = 3951;
const C8: u32 = 4186;

const SUPER_MARIO: &[Note] = &[
    n(E7, 150),
    n(E7, 150),
    n(R, 150),
    n(E7, 150),
    n(R, 150),
    n(C7, 150),
    n(E7, 150),
    n(R, 150),
    n(G7, 150),
    n(R, 450),
    n(G6, 150),
    n(R, 450),
    n(C7, 150),
    n(R, 300),
    n(G6, 150),
    n(R, 300),
    n(E6, 150),
    n(R, 300),
    n(A6, 150),
    n(B6, 150),
    n(AS6, 150),
    n(A6, 150),
    n(G6, 100),
    n(E7, 100),
    n(G7, 100),
    n(A7, 150),
    n(F7, 150),
    n(G7, 150),
    n(R, 150),
    n(E7, 150),
    n(C7, 150),
    n(D7, 150),
    n(B6, 150),
    n(R, 300),
];

const ZELDAS_LULLABY: &[Note] = &[
    n(B6, 300),
    n(D7, 300),
    n(A6, 600),
    n(R, 200),
    n(B6, 300),
    n(D7, 300),
    n(A6, 600),
    n(R, 200),
    n(B6, 300),
    n(D7, 300),
    n(A7, 300),
    n(G7, 300),
    n(D7, 600),
    n(C7, 200),
    n(B6, 200),
    n(A6, 600),
    n(R, 300),
];

// Tetris Type A follows the public-domain Russian folk melody Korobeiniki.
const TETRIS_TYPE_A: &[Note] = &[
    n(E7, 200),
    n(B6, 100),
    n(C7, 100),
    n(D7, 200),
    n(C7, 100),
    n(B6, 100),
    n(A6, 200),
    n(A6, 100),
    n(C7, 100),
    n(E7, 200),
    n(D7, 100),
    n(C7, 100),
    n(B6, 300),
    n(C7, 100),
    n(D7, 200),
    n(E7, 200),
    n(C7, 200),
    n(A6, 200),
    n(A6, 300),
    n(R, 100),
    n(D7, 200),
    n(F7, 100),
    n(A7, 200),
    n(G7, 100),
    n(F7, 100),
    n(E7, 300),
    n(C7, 100),
    n(E7, 200),
    n(D7, 100),
    n(C7, 100),
    n(B6, 200),
    n(B6, 100),
    n(C7, 100),
    n(D7, 200),
    n(E7, 200),
    n(C7, 200),
    n(A6, 200),
    n(A6, 300),
    n(R, 200),
];

const PAC_MAN_INTRO: &[Note] = &[
    n(B6, 80),
    n(B7, 80),
    n(FS7, 80),
    n(DS7, 80),
    n(B7, 40),
    n(FS7, 120),
    n(DS7, 160),
    n(C7, 80),
    n(C8, 80),
    n(G7, 80),
    n(E7, 80),
    n(C8, 40),
    n(G7, 120),
    n(E7, 160),
    n(B6, 80),
    n(B7, 80),
    n(FS7, 80),
    n(DS7, 80),
    n(B7, 40),
    n(FS7, 120),
    n(DS7, 160),
    n(DS7, 60),
    n(E7, 60),
    n(F7, 60),
    n(F7, 60),
    n(FS7, 60),
    n(G7, 60),
    n(G7, 60),
    n(GS6, 60),
    n(A6, 60),
    n(B6, 180),
    n(R, 200),
];

const FINAL_FANTASY_VICTORY: &[Note] = &[
    n(C7, 120),
    n(C7, 120),
    n(C7, 120),
    n(C7, 360),
    n(GS6, 360),
    n(AS6, 360),
    n(C7, 180),
    n(R, 60),
    n(AS6, 120),
    n(C7, 600),
    n(R, 120),
    n(E7, 180),
    n(D7, 180),
    n(E7, 180),
    n(G7, 540),
    n(E7, 180),
    n(D7, 180),
    n(C7, 540),
    n(R, 240),
];

pub const GAME_TUNES: &[Tune] = &[
    Tune {
        title: "Super Mario Bros. — overworld",
        notes: SUPER_MARIO,
    },
    Tune {
        title: "Ocarina of Time — Zelda's Lullaby",
        notes: ZELDAS_LULLABY,
    },
    Tune {
        title: "Tetris — Type A",
        notes: TETRIS_TYPE_A,
    },
    Tune {
        title: "Pac-Man — intro",
        notes: PAC_MAN_INTRO,
    },
    Tune {
        title: "Final Fantasy — victory fanfare",
        notes: FINAL_FANTASY_VICTORY,
    },
];
