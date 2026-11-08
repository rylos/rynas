#!/bin/sh
# OUTLINE (not run by CI yet): PC backup receive against a real btrbk, in throwaway
# containers only. Never on the host: the NAS side creates a system account, a sudoers file
# and btrfs subvolumes.
#
# NAS container: Debian trixie, privileged, container=docker, getty units masked (they would
# take tty1), a loop-mounted btrfs image on /volume1, the rynas .deb from build-deb.sh, sshd.
# PC container: Debian trixie with btrbk >= 0.32 and btrfs-progs, its own loop btrfs image.
#
# Steps:
#  1. NAS: start rynasd; POST /v1/pc-backups/targets {"device":"pc","path":"/volume1/backup/pc",
#     "public_key":"<PC key>"}; wait for the task.
#     Check: sudo -l -U rynas-pcbackup lists only the /volume1/backup/pc/ forms;
#            visudo -c passes; ~rynas-pcbackup/.ssh/authorized_keys has one restrict,command= line.
#  2. PC: btrbk.conf from GET /v1/pc-backups/targets/pc (client_config), then
#     `btrbk -v run` twice (full, then incremental).
#     Check: two received snapshots per subvolume on the NAS; auth log has ACCEPT lines only.
#  3. PC, by hand with the PC key (each must print "rynas: refused" and log REJECT):
#       ssh rynas-pcbackup@nas "btrfs receive '/volume1/backup/pc/<snapshot>/link'"
#       ssh rynas-pcbackup@nas "btrfs subvolume list -a /volume1"
#       ssh rynas-pcbackup@nas "btrfs receive '/volume1/backup/pc/' | sh"
#  4. NAS: replace /volume1/backup with a symlink to another folder holding pc/
#     -> the next `btrbk run` fails with "is a symlink" (verify_root); put it back.
#  5. Future names: on the NAS, `btrfs subvolume snapshot -r` a received snapshot as
#     home.20991231T0000 (keep_last 1) -> btrbk's prune of the real newest one is refused.
#  6. Two `btrbk run` at once from the PC -> the second receive is refused ("another receive").
#  7. DELETE /v1/pc-backups/targets/pc -> the account, its home and the sudoers file are gone;
#     /volume1/backup/pc is root:root 0750 with its snapshots.
#  8. apt purge rynas -> no rynas-pcbackup account or group, no /etc/sudoers.d/rynas-pcbackup.
set -eu
echo "outline only: see the comments in $0 and docs/research/pc-backup.md §10.3" >&2
exit 2
