/* dotz — CHAT panel wirer (re-export). Split from app.js (C7). No behavior change.
 * wireChatPanel lives in chat.js (alongside the transcript/composer/tool-card logic it shares);
 * this file re-exports it so panels.js can import every wirer from panels/*.js uniformly.
 */
export { wireChatPanel } from '../chat.js';