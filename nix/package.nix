{ pkgs, packages }:

let
  src = ../.;
  cargo = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package;
  sourceDateEpoch = "1";

  mkArchive =
    target: package:
    pkgs.runCommand "${cargo.name}-v${cargo.version}-${target}.tar.gz" { } ''
      export SOURCE_DATE_EPOCH=${sourceDateEpoch}

      mkdir staging
      cp ${package}/bin/${cargo.name} staging/
      cp ${src}/README.md ${src}/LICENSE ${src}/CHANGELOG.md staging/

      ${pkgs.gnutar}/bin/tar \
        --sort=name \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --clamp-mtime \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --format=posix \
        --pax-option=delete=atime,delete=ctime \
        --use-compress-program="${pkgs.gzip}/bin/gzip -n" \
        -C staging \
        -cf $out \
        .
    '';

  archives = builtins.mapAttrs mkArchive packages;
  copyArchives = pkgs.lib.concatMapStrings (archive: "cp ${archive} $out/${archive.name}\n") (
    builtins.attrValues archives
  );
  releaseArtifacts = pkgs.runCommand "${cargo.name}-release-artifacts" { } ''
    mkdir $out
    ${copyArchives}
  '';
in
{
  inherit archives releaseArtifacts;
}
