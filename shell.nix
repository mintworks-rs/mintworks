{ pkgs ? import <nixpkgs> {} }:
	pkgs.mkShell {
		# `bindgenHook` is the reason this file exists: the `libxml` crate (saas-nav's
		# dev-tests) generates its FFI with bindgen, which needs libclang and the right
		# `-isystem` flags, not just libxml2's headers on disk. It sets LIBCLANG_PATH and
		# BINDGEN_EXTRA_CLANG_ARGS; without it the build dies on "Unable to find libclang".
		nativeBuildInputs = with pkgs; [ pkg-config rustPlatform.bindgenHook ];

		# Linked against, so `buildInputs`: that is what puts libxml2 on PKG_CONFIG_PATH.
		# `openssl` is not optional: `webauthn-rs-core` (saas-auth's passkeys) links it
		# unconditionally, and nothing else in the workspace uses it.
		buildInputs = with pkgs; [ libxml2 openssl ];

		# Renice the shell, not each command: every build process is its child, so `cargo`,
		# 12 rustc/clippy-driver, the test binaries and `.git/hooks/pre-commit` all inherit it.
		# Absolute path, because `nix-shell --pure` leaves the system profile off PATH.
		shellHook = ''
			# Own target dir: this env (PKG_CONFIG_PATH, LIBCLANG_PATH, openssl) differs from an
			# editor's build outside the shell, so a shared `target/` rebuilt everything per switch.
			export CARGO_TARGET_DIR="$PWD/target/nix-shell"
			${pkgs.util-linux}/bin/renice -n 19 -p $$ >/dev/null
		'';
	}

# vim: ts=4
