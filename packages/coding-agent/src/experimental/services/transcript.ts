import type { LaneTranscriptSnapshot, LaneWatchEvent } from "@liuxuedeng/pi-core-agent";
import { defineService, type ReplicatedState } from "@liuxuedeng/pi-core-chord";

export interface TranscriptState {
	snapshot: LaneTranscriptSnapshot | null;
	/** The source event is retained for presentation side effects; hydration does not replay it. */
	event: LaneWatchEvent | null;
}

/** Coherent main-lane state replicated through Chord's operation stream. */
export interface Transcript {
	readonly state: ReplicatedState<TranscriptState>;
}

export const Transcript = defineService<Transcript>("pi.transcript");
