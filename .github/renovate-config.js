// Renovate (self-hosted, .github/workflows/renovate.yml) for the app catalogue in apps/:
// images stay pinned (tag@sha256), patch and minor updates are merged by themselves once the
// catalogue tests pass, major ones wait for a person. The published catalogue follows
// (.github/workflows/publish-catalog.yml). Design: docs/phase2/docker-apps.md § 6.13.
module.exports = {
  platform: "github",
  repositories: ["rylos/rynas"],
  onboarding: false,
  requireConfig: "optional",
  gitAuthor: "Marco Ziliani <508111+rylos@users.noreply.github.com>",
  dependencyDashboard: true,
  dependencyDashboardTitle: "App catalogue updates",
  enabledManagers: ["docker-compose"],
  "docker-compose": {
    managerFilePatterns: ["/^apps/[^/]+/compose\\.yaml$/"],
  },
  pinDigests: true,
  prConcurrentLimit: 10,
  // Renovate merges by itself once the checks passed (GitHub's own auto-merge would merge
  // at once without branch protection)
  platformAutomerge: false,
  prHourlyLimit: 0,
  allowedCommands: ["^packaging/renovate-bump\\.sh$"],
  packageRules: [
    {
      matchManagers: ["docker-compose"],
      // the entry's version and app_version follow the image (once per branch)
      postUpgradeTasks: {
        commands: ["packaging/renovate-bump.sh"],
        fileFilters: ["apps/*/app.toml"],
        executionMode: "branch",
      },
      commitMessagePrefix: "Apps:",
    },
    {
      // three days before a release can be merged by itself: a broken or hijacked tag is
      // usually pulled or fixed upstream within that time, before it reaches any NAS
      matchUpdateTypes: ["patch", "minor", "digest", "pinDigest"],
      automerge: true,
      automergeType: "pr",
      minimumReleaseAge: "3 days",
    },
    {
      matchUpdateTypes: ["major"],
      automerge: false,
      labels: ["catalogue-major"],
    },
    {
      // calendar versions (YYYY.M.patch): a new month can break things, like a major
      matchPackageNames: [
        "ghcr.io/home-assistant/home-assistant",
        "cloudflare/cloudflared",
      ],
      versioning: "regex:^(?<major>\\d{4})\\.(?<minor>\\d{1,2})\\.(?<patch>\\d+)$",
    },
    {
      matchPackageNames: [
        "ghcr.io/home-assistant/home-assistant",
        "cloudflare/cloudflared",
      ],
      matchUpdateTypes: ["minor"],
      automerge: false,
      labels: ["catalogue-major"],
    },
    {
      // Immich: server and machine learning always move together
      matchPackageNames: ["ghcr.io/immich-app/immich-server", "ghcr.io/immich-app/immich-machine-learning"],
      groupName: "immich",
    },
    {
      // quay garbage-collects manifests a tag no longer points at: never pin there
      matchPackageNames: ["quay.io/**"],
      pinDigests: false,
    },
  ],
};
