# An outbound allowlist for the VM's user (docs/nixos-vm.md, Network policy): the host, the chief, every
# agent and the user's rootless containers reach only the listed domains, on the listed ports.
#
# dnsmasq becomes the guest's resolver. It forwards only listed domains, to Lima's resolver (which asks the
# Mac's, and so its VPN's), and refuses every other name, so a lookup cannot carry data out either. Each
# address it hands out for a listed domain goes into an nftables set, and nftables lets the user's new
# connections through only to those addresses. Root and system services are not filtered, but resolve
# through the same dnsmasq. Without atomics.vm.hardened an agent can become root and lift all of this.
{ config, lib, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm.egress;
  uid = toString config.users.users.${config.atomics.vm.user}.uid;
  table = "atomics_egress";
  rules = [ { name = "web"; ports = cfg.webPorts; domains = cfg.domains; } ]
    ++ lib.imap0 (i: rule: rule // { name = "rule${toString i}"; }) cfg.rules;
  domains = lib.unique (lib.concatMap (rule: rule.domains) rules);
  ports = list: lib.concatMapStringsSep ", " toString list;
  sets = rule: ''
    set ${rule.name}4 { type ipv4_addr; flags timeout; timeout 1d; }
    set ${rule.name}6 { type ipv6_addr; flags timeout; timeout 1d; }
  '';
  allow = rule: ''
    tcp dport { ${ports rule.ports} } ip daddr @${rule.name}4 accept
    tcp dport { ${ports rule.ports} } ip6 daddr @${rule.name}6 accept
  '';

  # What any agent in this VM needs: its own service, GitHub, the common package registries and the
  # documentation sites agents read. A team adds its own (cloud, tickets, databases) with `domains` and
  # `rules`. Each entry includes its subdomains.
  defaults = [
    # Claude Code and Codex
    "anthropic.com" "claude.ai" "claude.com" "openai.com" "chatgpt.com"
    # GitHub, its packages, release downloads and raw files
    "github.com" "githubusercontent.com" "githubassets.com" "ghcr.io"
    # Package registries and toolchains
    "registry.npmjs.org" "npmjs.com" "nodejs.org" "yarnpkg.com"
    "api.nuget.org" "globalcdn.nuget.org" "nuget.org"
    "builds.dotnet.microsoft.com" "dotnetcli.azureedge.net" "dotnetcli.blob.core.windows.net" "aka.ms"
    "crates.io" "static.rust-lang.org" "rust-lang.org" "docs.rs"
    "pypi.org" "pythonhosted.org"
    "docker.io" "docker.com" "mcr.microsoft.com"
    "nixos.org"
    # Packages and downloads that image builds fetch
    "deb.debian.org" "security.debian.org" "archive.ubuntu.com" "security.ubuntu.com" "ports.ubuntu.com"
    "packages.microsoft.com" "download.microsoft.com" "go.microsoft.com"
    # Documentation
    "learn.microsoft.com" "docs.microsoft.com" "devblogs.microsoft.com"
    "stackoverflow.com" "stackexchange.com" "sstatic.net" "superuser.com" "serverfault.com" "askubuntu.com"
    "developer.mozilla.org" "typescriptlang.org" "web.dev" "caniuse.com" "lit.dev" "playwright.dev"
  ];
in
{
  options.atomics.vm.egress = {
    enable = lib.mkEnableOption ''
      an outbound allowlist for the VM's user: the host, the chief, agents and their rootless containers reach
      only listed domains. Pair it with atomics.vm.hardened, or an agent can become root and lift it
    '';

    enforce = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Refuse connections outside the list. False only logs them ("atomics-egress refused" in the kernel log,
        `journalctl -k -g atomics-egress`), to learn what a team needs before enforcing. Names outside the list
        are refused by dnsmasq either way.
      '';
    };

    defaults = mkOption {
      type = types.bool;
      default = true;
      description = "Allow ATOMICS's default domains: the agents' services, GitHub, package registries and documentation.";
    };

    domains = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "dev.azure.com" "visualstudio.com" ];
      description = "Domains, with their subdomains, the user may reach on `webPorts`. Definitions add up.";
    };

    webPorts = mkOption {
      type = types.listOf types.port;
      default = [ 80 443 ];
      description = "The ports `domains` are allowed on.";
    };

    rules = mkOption {
      type = types.listOf (types.submodule {
        options = {
          domains = mkOption {
            type = types.listOf types.str;
            description = "Domains, with their subdomains.";
          };
          ports = mkOption {
            type = types.listOf types.port;
            description = "The TCP ports allowed to them.";
          };
        };
      });
      default = [ ];
      example = [ { domains = [ "db.example.database.windows.net" ]; ports = [ 1433 ]; } ];
      description = "Other ports for some domains, such as a database's.";
    };

    upstream = mkOption {
      type = types.str;
      default = "192.168.5.2";
      description = "The resolver listed names are forwarded to: Lima's, on its user-mode network's gateway.";
    };
  };

  config = lib.mkIf cfg.enable {
    warnings = lib.optional (!config.atomics.vm.hardened)
      "atomics.vm.egress without atomics.vm.hardened: any agent can become root and lift the network policy.";

    atomics.vm.egress.domains = lib.mkIf cfg.defaults defaults;

    services.dnsmasq = {
      enable = true;
      resolveLocalQueries = true;
      settings = {
        listen-address = [ "127.0.0.1" ];
        bind-interfaces = true;
        # Only the listed domains have a server; every other name is refused.
        no-resolv = true;
        server = map (domain: "/${domain}/${cfg.upstream}") domains;
        nftset = lib.concatMap
          (rule: map (domain: "/${domain}/4#inet#${table}#${rule.name}4,6#inet#${table}#${rule.name}6") rule.domains)
          rules;
        domain-needed = true;
        bogus-priv = true;
        cache-size = 10000;
        # Answers are asked for again within the hour, which renews their addresses in the sets.
        max-cache-ttl = 3600;
      };
    };
    # dnsmasq alone in resolv.conf: glibc asks the next server after a refusal, and Lima's would answer.
    networking.dhcpcd.extraConfig = "nohook resolv.conf";
    # dnsmasq adds to sets that must exist first.
    systemd.services.dnsmasq = {
      after = [ "nftables.service" ];
      requires = [ "nftables.service" ];
    };

    networking.nftables.enable = true;
    networking.nftables.tables.${table} = {
      family = "inet";
      content = ''
        ${lib.concatMapStrings sets rules}
        chain output {
          type filter hook output priority filter; policy accept;
          meta skuid != ${uid} accept
          oifname "lo" accept
          ct state established,related accept
          ${lib.concatMapStrings allow rules}
          limit rate 20/second burst 50 packets log prefix "atomics-egress refused: " level info
          ${if cfg.enforce then ''
            meta l4proto tcp reject with tcp reset
            reject with icmpx type admin-prohibited
          '' else "accept"}
        }
      '';
    };
  };
}
