"""SWE-bench environment library for Apiary sandboxes.

Three layers of abstraction, from low to high:

``ApiaryClient``
    Thin HTTP wrapper around the Apiary daemon REST API (sessions, tasks,
    health checks).

``RootfsManager``
    Exports Docker images to rootfs directories on disk with caching, so
    the same image is never exported twice.

``SWEBenchSession``
    Combines both into a single entry point: pass a Docker image name, get
    a session you can ``execute()`` commands in.

Quick start::

    from apiary_swebench import SWEBenchSession

    with SWEBenchSession(image="swebench/sweb.eval.x86_64.django_1776_django-12345:latest") as s:
        result = s.execute("python -c 'import django; print(django.__version__)'")
        print(result.stdout)
"""

from apiary_swebench.client import ApiaryClient, TaskResult
from apiary_swebench.rootfs import RootfsManager
from apiary_swebench.session import SWEBenchSession

__all__ = ["ApiaryClient", "TaskResult", "RootfsManager", "SWEBenchSession"]
