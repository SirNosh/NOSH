# NOSH Pi package

This package is the sole model-runtime integration for NOSH Research. It contributes the bounded NOSH tools, worker/Reviewer/Director prompts, and the `nosh-control` skill to Pi.

Install it through Pi's package configuration by adding this directory or the Git repository package path. `nosh doctor` checks the compatible Pi version, package metadata, and provider-authentication presence without reading credential values.

Workers may submit typed records or request delegation; they cannot create child workers. Only `noshd` validates submissions and mutates durable state.
