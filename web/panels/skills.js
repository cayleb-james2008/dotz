/* dotz — SKILLS panel wirer. Split from app.js (C7). No behavior change. */
import { $, el, api, truncate } from '../api.js';
import { state } from '../state.js';
import { pushError } from '../chat.js';

function wireSkillsPanel(node) {
  const search = node.querySelector("#skills-search");
  search.addEventListener("input", () => renderSkillsList(search.value.toLowerCase()));
  refreshSkills();
}

async function refreshSkills() {
  try {
    const { skills } = await api("/api/skills");
    state.skills = skills || [];
    renderSkillsList("");
  } catch (e) { pushError("skills load: " + e.message); }
}

function renderSkillsList(filter) {
  const panel = document.querySelector('.panel[data-panel="skills"]');
  const list = panel ? panel.querySelector("#skills-list") : $("skills-list");
  const count = panel ? panel.querySelector("#skills-count") : $("skills-count");
  if (!list || !count) return;
  const filtered = filter ? state.skills.filter((s) => s.name.toLowerCase().includes(filter) || (s.description || "").toLowerCase().includes(filter)) : state.skills;
  count.textContent = `${filtered.length} of ${state.skills.length} skills`;
  list.innerHTML = "";
  filtered.slice(0, 200).forEach((s) => {
    const item = el("div", "skill-item");
    const head = el("div", "skill-item-head");
    head.appendChild(el("span", "skill-name", s.name + (s.isUmbrella ? " ◫" : "")));
    head.appendChild(el("span", "skill-source " + s.source, s.source));
    item.appendChild(head);
    item.appendChild(el("div", "skill-desc", truncate(s.description, 100)));
    item.onclick = () => showSkillDetail(s);
    list.appendChild(item);
  });
}

async function showSkillDetail(skill) {
  const panel = document.querySelector('.panel[data-panel="skills"]');
  const detail = panel ? panel.querySelector("#skills-detail") : $("skills-detail");
  if (!detail) return;
  detail.classList.remove("hidden");
  detail.innerHTML = "";
  const head = el("div", "skills-detail-head");
  head.appendChild(el("span", "skill-name", skill.name));
  head.appendChild(el("span", "skill-source " + skill.source, skill.source));
  const close = el("button", "skills-detail-close", "×");
  close.onclick = () => detail.classList.add("hidden");
  head.appendChild(close);
  detail.appendChild(head);
  const body = el("div", "skills-detail-body", "loading…");
  detail.appendChild(body);
  try {
    const { body: full } = await api(`/api/skills/${encodeURIComponent(skill.name)}`);
    body.textContent = full;
  } catch (e) { body.textContent = "error: " + e.message; }
}

export { wireSkillsPanel, refreshSkills, renderSkillsList, showSkillDetail };