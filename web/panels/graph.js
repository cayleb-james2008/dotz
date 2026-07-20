/* dotz — WORKFLOW GRAPH panel wirer (re-export). Split from app.js (C7). No behavior change.
 * wireGraphPanel lives in graph.js (alongside renderWorkflowDag / showNodeDetail / the Q2 render
 * signature guard + Q3 PANEL_REGISTRY-derived chip colors / C5 viewport culling it shares);
 * this file re-exports it so panels.js can import every wirer from panels/*.js uniformly.
 */
export { wireGraphPanel } from '../graph.js';