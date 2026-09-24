import { name } from './cycle-a.mjs';
export function fromB() { return 'b, b sees ' + name(); }
