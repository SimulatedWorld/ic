#!/usr/bin/env python3
#
# Packs contents of a tar file into a ext4 image (possibly taking only a
# subdirectory of the full tar file). The (sparse) ext4 image itself is then
# wrapped into a tzst file.
#
# Call example:
#   build_ext4_image -s 10M -o partition.img.tzst -p boot -i dockerimg.tar -S file_contexts
#
import argparse
import os
import subprocess
import sys
import tempfile


def limit_file_contexts(file_contexts, base_path):
    r"""
    Projects file contexts to given base path.

    Takes an SELinux filecontext file describing the entire
    system and extracts all statements that refer to the given
    base_path. All statements not relating to that base_path are
    deleted, and all paths are adjusted such that the common
    path prefix is removed.

    Example: given this file_context input:

        /.*     system_u:object_r:default_t:s0
        /a?quota\.(user|group)  --      system_u:object_r:quota_db_t:s0
        /boot/.*        system_u:object_r:boot_t:s0
        /boot/efi(/.*)?/System\.map(-.*)?       --      system_u:object_r:system_map_t:s0
        /boot/lost\+found/.*    <<none>>


    and base_path="/boot/" will produce this output:

        .*        system_u:object_r:boot_t:s0
        efi(/.*)?/System\.map(-.*)?       --      system_u:object_r:system_map_t:s0

    """
    lines = []
    for line in file_contexts.split("\n"):
        # Drop all statements assigning no label at all
        if line.find("<<none>>") != -1:
            continue
        if base_path:
            if line.startswith(base_path):
                lines.append(line[len(base_path) :])
        else:
            lines.append(line)
    return "\n".join(lines) + "\n"


def get_root_context(file_contexts, base_path):
    for line in file_contexts.split("\n"):
        pieces = line.split()
        if len(pieces) >= 3 and pieces[0] == base_path and pieces[1] == "-d":
            return pieces[2]
    raise RuntimeError("Could determine SELinux context for root inode")


def read_fakeroot_state(statefile):
    """
    Reads fakeroot state file, key by inode.

    fakeroot state file has the following structure:
        dev=fd03,ino=136887,mode=100600,uid=0,gid=0,nlink=1,rdev=0
        dev=fd02,ino=3932193,mode=100600,uid=0,gid=0,nlink=1,rdev=0

    Read all fields by line into a dictionary, and keep this
    dictionary keyed by inode number.
    """
    entry_by_inode = {}
    for line in open(statefile, "r").readlines():
        d = {}
        for item in line[:-1].split(","):
            k, v = item.split("=")
            d[k] = v
        entry_by_inode[int(d["ino"])] = d
    return entry_by_inode


def strip_files(fs_basedir, fakeroot_statefile, strip_paths):
    flattened_paths = []
    for path in strip_paths:
        if path[0] == "/":
            path = path[1:]

        target_path = os.path.join(fs_basedir, path)
        if os.path.isdir(target_path):
            for entry in os.listdir(target_path):
                del_path = os.path.join(target_path, entry)
                flattened_paths.append(del_path)
        else:
            flattened_paths.append(target_path)

    # TODO: replace this with itertools.batched when we have Python 3.12
    BATCH_SIZE = 100
    for batch_start in range(0, len(flattened_paths), BATCH_SIZE):
        batch_end = min(batch_start + BATCH_SIZE, len(flattened_paths))
        subprocess.run(
            ["fakeroot", "-s", fakeroot_statefile, "-i", fakeroot_statefile, "rm", "-rf"]
            + flattened_paths[batch_start:batch_end],
            check=True,
        )


def prepare_tree_from_tar(in_file, fakeroot_statefile, fs_basedir, dir_to_extract, extra_files):
    # We batch all commands together and run them under bash. This is significantly faster than invoking fakeroot
    # multiple times.
    commands = "set -euo pipefail\n"
    if in_file:
        # Untar files to the base dir.
        commands += f"""tar xf {in_file} --numeric-owner -C "{fs_basedir}" "{dir_to_extract}";\n"""

        # Copy extra files to the base dir and set permissions.
        for path_target in extra_files or []:
            (path, target, mod) = path_target.split(":")
            target_in_basedir = os.path.join(fs_basedir, dir_to_extract, target.lstrip("/"))
            commands += f"""cp "{path}" "{target_in_basedir}";\n"""
            commands += f"""chmod "{mod}" "{target_in_basedir}";\n"""
    else:
        commands += f"""chown root:root "{fs_basedir}";\n"""

    subprocess.run(["fakeroot", "-s", fakeroot_statefile, "bash"], input=commands.encode(), check=True)


def make_argparser():
    parser = argparse.ArgumentParser()
    parser.add_argument("-s", "--size", help="Size of image to build", type=str)
    parser.add_argument("-o", "--output", help="Target (tzst) file to write partition image to", type=str)
    parser.add_argument(
        "--extra-files",
        help="Extra files to inject into the image. Format: source_path:target_path_in_image:target_permissions",
        nargs="*",
    )
    parser.add_argument(
        "-i", "--input", help="Source (tar) file to take files from", type=str, default="", required=False
    )
    parser.add_argument(
        "-p",
        "--path",
        help="Path to extract from tar file (only files below will be put into image)",
        required=False,
        default="",
        type=str,
    )
    parser.add_argument(
        "-S",
        "--file_contexts",
        help="Path to SELinux file_contexts description file",
        required=False,
        default="",
        type=str,
    )
    parser.add_argument(
        "--strip-paths",
        metavar="strip_paths",
        type=str,
        nargs="*",
        default=[],
        help="Directories to be cleared from the tree; expects a list of full paths",
    )
    parser.add_argument("--dflate", help="Path to our dflate tool", type=str, required=True)
    parser.add_argument("--diroid", help="Path to our diroid tool", type=str, required=True)
    return parser


def main():
    args = make_argparser().parse_args(sys.argv[1:])

    in_file = args.input
    out_file = args.output
    image_size = args.size
    limit_prefix = args.path
    extra_files = args.extra_files
    file_contexts_file = args.file_contexts
    strip_paths = args.strip_paths
    if limit_prefix and limit_prefix[0] == "/":
        limit_prefix = limit_prefix[1:]

    tmpdir = tempfile.mkdtemp()

    if file_contexts_file:
        original_file_contexts = open(file_contexts_file, "r").read()
        if limit_prefix:
            prefix = "/" + limit_prefix
        else:
            prefix = ""
        file_contexts = limit_file_contexts(original_file_contexts, prefix)
        file_contexts_file = os.path.join(tmpdir, "file_contexts")
        open(file_contexts_file, "w").write(file_contexts)

    fs_basedir = os.path.join(tmpdir, "fs")
    fakeroot_statefile = os.path.join(tmpdir, "fakeroot.state")
    os.mkdir(fs_basedir)
    image_file = os.path.join(tmpdir, "partition.img")

    # Prepare a filesystem tree that represents what will go into
    # the fs image. Wrap everything in fakeroot so permissions and
    # ownership will be preserved while unpacking (see below).
    prepare_tree_from_tar(in_file, fakeroot_statefile, fs_basedir, limit_prefix, extra_files)
    strip_files(fs_basedir, fakeroot_statefile, strip_paths)
    subprocess.run(["sync"], check=True)

    # Now build the basic filesystem image. Wrap again in fakeroot
    # so correct permissions are read for all files etc.
    mke2fs_args = [
        "faketime",
        "-f",
        "1970-1-1 0:0:0",
        "/usr/sbin/mkfs.ext4",
        "-E",
        "hash_seed=c61251eb-100b-48fe-b089-57dea7368612",
        "-U",
        "clear",
        "-d",
        os.path.join(fs_basedir, limit_prefix),
        "-F",
        image_file,
        str(image_size),
    ]
    subprocess.run(mke2fs_args, check=True, env={"E2FSPROGS_FAKE_TIME": "0"})

    # Use our tool, diroid, to create an fs_config file to be used by e2fsdroid.
    # This file is a simple list of files with their desired uid, gid, and mode.
    fs_config_path = os.path.join(tmpdir, "fs_config")
    diroid_args = [
        args.diroid,
        "--fakeroot",
        fakeroot_statefile,
        "--input-dir",
        os.path.join(fs_basedir, limit_prefix),
        "--output",
        fs_config_path,
    ]
    subprocess.run(diroid_args, check=True)

    e2fsdroid_args = [
        "faketime",
        "-f",
        "1970-1-1 0:0:0",
        "fakeroot",
        "-i",
        fakeroot_statefile,
        "e2fsdroid",
        "-e",
        "-a",
        "/",
        "-T",
        "0",
    ]
    e2fsdroid_args += ["-C", fs_config_path]
    if file_contexts_file:
        e2fsdroid_args += ["-S", file_contexts_file]
    e2fsdroid_args += [image_file]
    subprocess.run(e2fsdroid_args, check=True, env={"E2FSPROGS_FAKE_TIME": "0"})

    subprocess.run(["sync"], check=True)

    # We use our tool, dflate, to quickly create a sparse, deterministic, tar.
    # If dflate is ever misbehaving, it can be replaced with:
    # tar cf <output> --sort=name --owner=root:0 --group=root:0 --mtime="UTC 1970-01-01 00:00:00" --sparse --hole-detection=raw -C <context_path> <item>
    temp_tar = os.path.join(tmpdir, "partition.tar")
    subprocess.run(
        [
            args.dflate,
            "--input",
            image_file,
            "--output",
            temp_tar,
        ],
        check=True,
    )

    subprocess.run(
        [
            "zstd",
            "-q",
            "--threads=0",
            temp_tar,
            "-o",
            out_file,
        ],
        check=True,
    )

    # tempfile cleanup is handled by proc_wrapper.sh


if __name__ == "__main__":
    main()
