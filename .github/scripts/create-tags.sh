#!/usr/bin/env bash
# Create a semver tag (vX.Y.Z) for the newest commit of a ref (if it has none), whose
# version is worked out by counting every untagged commit on the first-parent history,
# bumping the version sequentially per commit message:
#
#   type!: ... / type(scope)!: ... / BREAKING CHANGE: footer  -> major (minor, patch = 0)
#   feat: ... / feat(scope): ...                              -> minor (patch = 0)
#   anything else                                             -> patch
#
# Existing version tags are respected: walking oldest to newest, a tagged
# commit resets the running version to its tag and discards untagged commits
# before it. Only the newest commit gets a tag: the commits before it count towards
# its version but are not tagged (or released) themselves, so a commit that can't be
# tagged, e.g. because it changed a workflow file, can't hold up the ones after it.
# The base version is 0.0.0 when no tag exists.
#
# Usage: create-tags.sh [ref]   (default HEAD)
# Tags are created locally (annotated); pushing is left to the caller.
# Env: DRY_RUN=1 only prints. If GITHUB_OUTPUT is set, writes `tags=<json array>`.
set -euo pipefail

ref=${1:-HEAD}
semver_re='^v([0-9]+)\.([0-9]+)\.([0-9]+)$'

# Highest version tag pointing at a commit, empty if none.
version_tag_at() {
  git tag --points-at "$1" --list 'v*' | grep -E "$semver_re" | sort -V | tail -n1 || true
}

bump_kind() {
  local message=$1 subject
  subject=$(head -n1 <<<"$message")
  if grep -Eq '^[a-zA-Z]+(\([^)]*\))?!:' <<<"$subject" ||
    grep -Eq '^BREAKING[ -]CHANGE:' <<<"$message"; then
    echo major
  elif grep -Eq '^feat(\([^)]*\))?:' <<<"$subject"; then
    echo minor
  else
    echo patch
  fi
}

major=0 minor=0 patch=0
pending=()

while read -r sha; do
  existing=$(version_tag_at "$sha")
  if [[ -n $existing ]]; then
    [[ $existing =~ $semver_re ]]
    major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}
    pending=()
    continue
  fi

  case $(bump_kind "$(git log -1 --format=%B "$sha")") in
    major) major=$((major + 1)) minor=0 patch=0 ;;
    minor) minor=$((minor + 1)) patch=0 ;;
    patch) patch=$((patch + 1)) ;;
  esac
  pending+=("$sha v$major.$minor.$patch")
done < <(git rev-list --reverse --first-parent "$ref")

tags=()
# Only the newest commit is tagged; the others are only counted.
for entry in ${pending[@]+"${pending[@]}"}; do
  read -r sha tag <<<"$entry"
  if ((${#pending[@]} > 1)) && [[ $entry != "${pending[${#pending[@]} - 1]}" ]]; then
    echo "$tag -> $(git log -1 --format='%h %s' "$sha") (counted, not tagged)"
    continue
  fi
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    echo "error: $tag already exists on another commit; cannot tag $sha" >&2
    exit 1
  fi
  echo "$tag -> $(git log -1 --format='%h %s' "$sha")"
  [[ ${DRY_RUN:-} == 1 ]] || git tag -a "$tag" "$sha" -m "$tag"
  tags+=("$tag")
done

json=""
for tag in ${tags[@]+"${tags[@]}"}; do json+="\"$tag\","; done
json="[${json%,}]"
echo "tags=$json"
[[ -z ${GITHUB_OUTPUT:-} ]] || echo "tags=$json" >>"$GITHUB_OUTPUT"
