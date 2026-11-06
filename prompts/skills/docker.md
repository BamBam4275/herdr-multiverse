# Docker — temporary container discipline

This playbook is for roles that may start short-lived Docker workloads. It is
deliberately a permissioned recipe: use it for temporary test/build containers,
never for production or another project's containers.

---

## Skill: docker

The runner exports the full task id as `MULTIVERSE_TASK_ID`. Use that value for
ownership, and derive the six-character suffix for the temporary name. Replace
`mv` with the project's own configured short code when working in another
configured project.

### One temporary container

```sh
TASK_ID="${MULTIVERSE_TASK_ID:?run this from a dispatched lane}"
TASK6="${TASK_ID: -6}"
docker run --rm \
  --name "mvt-mv-${TASK6}-test" \
  --label "multiverse.owner=multiverse/${TASK_ID}" \
  --label multiverse.temp=1 \
  IMAGE:TAG COMMAND
```

The name is `mvt-<project>-<task6>-<purpose>`. The owner label is the existing
`multiverse.owner=<project>/<full task id>` convention; do not invent separate
project or task labels.

### A temporary Compose project

```sh
TASK_ID="${MULTIVERSE_TASK_ID:?run this from a dispatched lane}"
TASK6="${TASK_ID: -6}"
docker compose -p "mvt-mv-${TASK6}" run --rm \
  --name "mvt-mv-${TASK6}-test" \
  -e "MULTIVERSE_TASK_ID=${TASK_ID}" \
  SERVICE COMMAND
```

The Compose project name must be `mvt-<project>-<task6>` and the one-shot run
must use `--rm`. Add the same `multiverse.owner=multiverse/<full task id>` and
`multiverse.temp=1` labels in the service definition when the Compose file
creates the container.

### Cleanup before finishing

`--rm` is required, but cleanup is still part of the lane's completion
protocol. Remove only this task's containers by their owner label, before
writing `done`:

```sh
TASK_ID="${MULTIVERSE_TASK_ID:?run this from a dispatched lane}"
IDS="$(docker ps -aq --filter "label=multiverse.owner=multiverse/${TASK_ID}")"
if [ -n "$IDS" ]; then docker rm -f $IDS; fi
```

Never kill by image, by name grep, or by a broad prune. Never touch a
production or another project's container. a nexus one-off has no lane
task id; use `mvt-<project>-orch-<purpose>` and its own explicit owner label.
