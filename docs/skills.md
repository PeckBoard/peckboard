---
title: Skills
nav_order: 7.5
has_children: true
has_toc: false
---

# Skills

Reusable agent instructions from the public catalog. Open one to read it, or subscribe from your PeckBoard. [How skills work](skills-guide.html)

<div id="skills-search" class="skills-search" data-index-url="https://raw.githubusercontent.com/PeckBoard/skills/main/index.json" markdown="0">
  <form class="skills-instance" id="skills-instance-form" data-testid="skills-instance-form" novalidate>
    <label for="skills-instance-url">Your PeckBoard address</label>
    <input id="skills-instance-url" data-testid="skills-instance-url" type="text" inputmode="url" autocomplete="url" spellcheck="false" placeholder="https://peckboard.example.com:3345" />
    <button type="submit" data-testid="skills-instance-save">Save</button>
    <span class="skills-instance-status" id="skills-instance-status" data-testid="skills-instance-status" role="status"></span>
  </form>
  <div class="skills-controls">
    <input id="skills-q" data-testid="skills-q" type="search" placeholder="Search by name, description, or tag" aria-label="Search skills" />
    <select id="skills-group" data-testid="skills-group" aria-label="Filter by group">
      <option value="">All groups</option>
    </select>
  </div>
  <p class="skills-status" id="skills-status" data-testid="skills-status" role="status">Loading the public catalog…</p>
  <ul class="skills-results" id="skills-results" data-testid="skills-results"></ul>
  <noscript><p>The search needs JavaScript. Browse the catalog on <a href="https://github.com/PeckBoard/skills">GitHub</a> instead.</p></noscript>
</div>
<script src="{{ '/assets/js/skills-search.js' | relative_url }}" defer></script>
