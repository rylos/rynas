PiGallery2 turns a shared folder of photos and videos into a fast web gallery with albums by
folder, a map, search on names and dates, and share links. It reads the folders as they are and
keeps its index in a small SQLite file, so it suits systems with little memory.

It does not recognize faces or objects, and it has no upload of its own: the photos stay plain
files in the shared folder, mounted read-only. To back up a phone, let Syncthing or a similar
app write into the same shared folder; new files appear after the next index.
