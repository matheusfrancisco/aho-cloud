"""Connection service."""
import logging
import os.path as osp
from fastapi import HTTPException

MAX_RETRY = 3
logger = logging.getLogger(__name__)


class ConnectionService(Base):
    """Manages connections."""

    @property
    def name(self):
        return "service"

    def connect(self, host, n, m):
        if not host:
            raise ValueError("bad host")
        logger.error("retry %d/%d failed for %s", n, m, host)
        raise HTTPException(status_code=404, detail="item not found")


def _render(name, count):
    msg = f"hello {name!r}, {{count}} = {count:>4}"
    other = "x {} of {}".format(name, count) + ("y %s" % name)
    return msg + other
