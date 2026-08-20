import { componentize } from '@bytecodealliance/componentize-js';
import { readFile, writeFile } from 'node:fs/promises';

const { component } = await componentize({
  sourcePath: 'src/guest.js',
  witPath: 'wit',
  worldName: 'guest',
  disableFeatures: ['stdio', 'random', 'clocks', 'http', 'fetch-event'],
});
await writeFile('/out/guest.wasm', component);
console.log(`wrote guest.wasm (${(component.length / 1048576).toFixed(2)} MiB)`);
