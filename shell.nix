{ pkgs ? import <nixpkgs> {} }:
	pkgs.mkShell {
		# `bindgenHook` is the reason this file exists: the `libxml` crate (saas-nav's
		# dev-tests) generates its FFI with bindgen, which needs libclang and the right
		# `-isystem` flags, not just libxml2's headers on disk. It sets LIBCLANG_PATH and
		# BINDGEN_EXTRA_CLANG_ARGS; without it the build dies on "Unable to find libclang".
		nativeBuildInputs = with pkgs; [ pkg-config rustPlatform.bindgenHook ];

		# Linked against, so `buildInputs`: that is what puts libxml2 on PKG_CONFIG_PATH.
		buildInputs = with pkgs; [ libxml2 ];
	}

# vim: ts=4
