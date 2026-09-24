import { fromB } from './cycle-b.mjs';
export function name() { return 'a'; }
export function fromA() { return 'a sees ' + fromB(); }
