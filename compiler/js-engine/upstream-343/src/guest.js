import { giveSigned } from 'example:signed/host-api';

let seen = 0n;
export function sinkS64(v) { seen = v; }
export function sourceS64() { return -3n; }
export function pullFromImport() { seen = giveSigned(); }      // lift only
export function pullAndReturn() { return giveSigned(); }        // lift then lower
export function sinkU64(v) { seen = v; }
