{ config, lib, pkgs, ... }:

# Declarative LXC domains for libvirt: Nix data in, libvirt domain XML out.
#
#   virtualisation.libvirtLxc.containers.ubuntu = {
#     vcpus  = 2;
#     memory = 5120;                            # MiB
#     swap   = 4096;                            # MiB on top of memory (see below)
#     rootfs = "/var/lib/lxc/ubuntu/rootfs";    # a directory with a real userland
#     mac    = "02:00:00:00:00:08";
#   };
#
# The Nix side produces a plain data tree (`tree`), `builtins.toXML` serialises
# it, and ./libvirt-domain.xsl maps that onto the libvirt schema at build time.
# No XML is written by hand: `nix eval --json ...tree` shows the data,
# `nix build ...xml` shows the result.
#
# The XML is installed as /var/lib/libvirt/lxc/<name>.xml with an autostart
# symlink beside it -- where libvirt's LXC driver looks
# (`virDomainObjListLoadAllConfigs` against `cfg->configDir`/`cfg->autostartDir`).
# Note it is not /etc/libvirt/lxc: nixpkgs builds libvirt with
# --sysconfdir=/var/lib.
#
# Deliberately not imported by hosts/nixos.nix at the time of writing: the
# incus variant lives in modules/lxc-ubuntu.nix and both would claim the same
# MAC on br-lan.  Enable one or the other.
let
  cfg = config.virtualisation.libvirtLxc;

  containerModule = { name, config, ... }: {
    options = {
      vcpus = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = "Visible CPUs, emitted as <vcpu placement='static'> (cpuset pinning).";
      };

      memory = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1024;
        description = "Memory limit in MiB; the stylesheet writes KiB.";
      };

      swap = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.unsigned;
        default = null;
        example = 4096;
        description = ''
          Extra swap allowance in MiB, emitted as <memtune><swap_hard_limit>.
          On a cgroup v2 host this is the part that needs verifying: there is no
          memsw controller, so whether libvirt maps it to `memory.swap.max` --
          and with which semantics -- has to be checked afterwards in
          /sys/fs/cgroup/system.slice/machine-<name>.scope/.
        '';
      };

      init = lib.mkOption {
        type = lib.types.str;
        default = "/sbin/init";
        description = "Init binary inside `rootfs`.";
      };

      rootfs = lib.mkOption {
        type = lib.types.str;
        description = ''
          Host directory holding the container's root filesystem.  It must
          contain a real userland; producing that is out of scope here
          (debootstrap, or unpacking a distro rootfs tarball).
        '';
      };

      bridge = lib.mkOption {
        type = lib.types.str;
        default = "br-lan";
        description = "Host bridge for the container interface.";
      };

      mac = lib.mkOption {
        type = lib.types.str;
        description = "Interface MAC (02: = locally administered).";
      };

      ifname = lib.mkOption {
        type = lib.types.str;
        default = "eth0";
        description = "Interface name inside the container.";
      };

      autostart = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Start the domain when libvirtd starts.";
      };

      extraXML = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "<hostdev mode='capabilities' type='misc'><source><char>net/tun</char></source></hostdev>" ];
        description = ''
          Raw XML appended inside <devices>, injected with
          disable-output-escaping.  This is the only unescaped channel, so keep
          it to markup you wrote yourself.
        '';
      };

      tree = lib.mkOption {
        type = lib.types.attrs;
        readOnly = true;
        description = ''
          The data handed to `builtins.toXML`.  Inspect with
          `nix eval --json ...tree`, feed to the stylesheet with
          `nix eval --json ...tree | toXML | xsltproc`.
        '';
      };

      xml = lib.mkOption {
        type = lib.types.package;
        readOnly = true;
        description = "Build result holding the generated libvirt domain XML.";
      };
    };

    # Null means "not set", and toXML would serialise it as <null/>, so drop it.
    config.tree = {
      inherit (config) vcpus memory init rootfs bridge mac ifname extraXML;
      inherit name;
    } // lib.optionalAttrs (config.swap != null) { inherit (config) swap; };

    config.xml = pkgs.runCommand "libvirt-lxc-${name}.xml" {
      nativeBuildInputs = [ pkgs.libxslt ];
      treeXML = pkgs.writeText "libvirt-lxc-${name}-tree.xml" (builtins.toXML config.tree);
    } ''
      xsltproc ${./libvirt-domain.xsl} "$treeXML" > $out
    '';
  };
in
{
  options.virtualisation.libvirtLxc.containers = lib.mkOption {
    type = lib.types.attrsOf (lib.types.submodule containerModule);
    default = { };
    description = "LXC domains to define for libvirtd, keyed by domain name.";
  };

  config = lib.mkIf (cfg.containers != { }) {
    virtualisation.libvirtd.enable = true;

    # libvirtd only reads its config directory at startup, so the definitions
    # must exist before it starts.  When a definition changes the unit re-runs
    # and bounces libvirtd so the new XML is loaded; running domains survive a
    # libvirtd restart (libvirt reconnects to them).
    #
    # There is no reconciliation for removals: dropping a container from the
    # attrset leaves its XML behind, and autostart will keep starting it until
    # the file is removed (or `virsh -c lxc:/// undefine <name>`).
    systemd.services = lib.mapAttrs' (
      name: c:
      lib.nameValuePair "libvirt-lxc-${name}" {
        description = "Define libvirt LXC domain ${name}";
        before = [ "libvirtd.service" ];
        wantedBy = [ "multi-user.target" ];
        restartTriggers = [ c.xml ];
        path = [ config.systemd.package pkgs.coreutils ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -Dm644 ${c.xml} /var/lib/libvirt/lxc/${name}.xml
          install -d /var/lib/libvirt/lxc/autostart
        ''
        + lib.optionalString c.autostart ''
          ln -sfn ../${name}.xml /var/lib/libvirt/lxc/autostart/${name}.xml
        ''
        + lib.optionalString (!c.autostart) ''
          rm -f /var/lib/libvirt/lxc/autostart/${name}.xml
        ''
        + ''
          systemctl try-restart libvirtd.service
        '';
      }
    ) cfg.containers;
  };
}
