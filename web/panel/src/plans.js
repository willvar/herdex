// Display-only labels matching Codex's detailed subscription status:
// https://github.com/openai/codex/blob/16e27626e654b29cc2ea696a545b2cd5f29db136/codex-rs/tui/src/subscription.rs
// Wire aliases: https://github.com/openai/codex/blob/16e27626e654b29cc2ea696a545b2cd5f29db136/codex-rs/protocol/src/auth.rs
const labels = new Map([
  ['free', 'Free'],
  ['go', 'Go'],
  ['plus', 'Plus'],
  ['prolite', 'Pro 100'],
  ['pro', 'Pro 200'],
  ['promax', 'Pro 500'],
  ['team', 'Business'],
  ['self_serve_business_prolite', 'Business Premium'],
  ['self_serve_business_usage_based', 'Business'],
  ['business', 'Enterprise'],
  ['ent26', 'Enterprise'],
  ['enterprise_cbp_automation', 'Enterprise (Automation)'],
  ['enterprise_cbp_usage_based', 'Enterprise'],
  ['enterprise', 'Enterprise'],
  ['hc', 'Enterprise'],
  ['edu', 'Edu'],
  ['education', 'Edu'],
  ['edu_plus', 'Edu Plus'],
  ['edu_pro', 'Edu Pro'],
  ['sci', 'Academic Researchers'],
  ['sci_plus', 'Academic Researchers Plus'],
  ['unknown', 'Unknown'],
])

export function planLabel(plan) {
  if (typeof plan !== 'string' || !plan) return '—'
  return labels.get(plan.toLowerCase()) ?? plan
}
