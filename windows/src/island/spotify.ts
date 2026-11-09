// Spotify's reports → the page (src-tauri/src/spotify.rs), and SoundCloud's
// the same way (src-tauri/src/soundcloud.rs). The parts of
// SpotifyController.swift that touch the app: the pill wears the track's
// title, music starting shows the compact island without a sound, and the
// card reads the player again whenever it comes on screen (seeks made in
// Spotify's own window are never signalled).

import { Bridge, onEvent } from "../core/bridge";
import { pillDefinition } from "../core/pills";
import { SOUNDCLOUD_ID, SPOTIFY_ID, SoundCloud, Spotify, isAd, type Player, type SpotifyState } from "../core/spotify";
import { State } from "../core/state";
import { t } from "../i18n/i18n";

export interface SpotifyHost {
  /** Compact island from hidden, no peek sound. */
  revealSilently(): void;
}

/** A music pill: its player on the page, its island events, how to read it again. */
interface Source {
  id: string;
  player: Player;
  event: string;
  refresh(): Promise<unknown>;
}

const SOURCES: Source[] = [
  { id: SPOTIFY_ID, player: Spotify, event: "spotify", refresh: () => Bridge.spotifyRefresh() },
  { id: SOUNDCLOUD_ID, player: SoundCloud, event: "soundcloud", refresh: () => Bridge.soundcloudRefresh() },
];

export function registerSpotifyHandlers(island: SpotifyHost) {
  for (const src of SOURCES) {
    void onEvent<SpotifyState>(src.event, (s) => applyPlayer(island, src, s));
    void onEvent<{ artUrl: string; dataUrl: string }>(`${src.event}-artwork`, (art) => {
      src.player.artwork = art;
      State.notify();
    });
  }
  const shown = new Set<string>();
  State.subscribe(() => {
    for (const src of SOURCES) {
      syncPillName(src);
      // Read again whenever its card comes on screen.
      const now = State.mode === "expanded" && State.view === "overview" && State.focusTask?.id === src.id;
      if (now && !shown.has(src.id)) void src.refresh();
      if (now) shown.add(src.id);
      else shown.delete(src.id);
    }
  });
}

/** A report from Rust (or the answer to a refresh). */
export function applySpotify(island: SpotifyHost, next: SpotifyState) {
  applyPlayer(island, SOURCES[0], next);
}

export function applySoundCloud(island: SpotifyHost, next: SpotifyState) {
  applyPlayer(island, SOURCES[1], next);
}

function applyPlayer(island: SpotifyHost, src: Source, next: SpotifyState) {
  const wasPlaying = State.spotifyPlaying;
  src.player.state = next;
  syncPillName(src);
  // Only on not playing → playing (SpotifyController.setPlaying).
  if (!wasPlaying && State.spotifyPlaying && !State.paused && State.mode === "hidden") {
    island.revealSilently();
  }
  State.notify();
}

/** SpotifyController.syncTaskName: the track's title, else the pill's name. */
export function syncPillName(src: Source = SOURCES[0]) {
  const task = State.tasks.find((x) => x.id === src.id);
  if (!task) return;
  const track = src.player.state.track;
  const title = isAd(track) ? t("Advertisement") : (track?.title ?? "");
  const name = title || (pillDefinition(src.id)?.name ?? "Spotify");
  if (task.name !== name) task.name = name;
}
