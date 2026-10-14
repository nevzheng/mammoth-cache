"""Reads the bucket behind the L3 tier, to assert what reached it."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import boto3
from botocore.config import Config
from botocore.exceptions import BotoCoreError, ClientError

if TYPE_CHECKING:
    from types_boto3_s3 import S3Client

    from mammoth_e2e.target import Bucket


@dataclass(frozen=True, slots=True)
class BucketInspector:
    bucket: Bucket
    client: S3Client

    @classmethod
    def connect(cls, bucket: Bucket) -> BucketInspector:
        client = boto3.client(
            "s3",
            endpoint_url=str(bucket.endpoint) if bucket.endpoint else None,
            region_name=bucket.region,
            aws_access_key_id=(
                bucket.access_key_id.get_secret_value() if bucket.access_key_id else None
            ),
            aws_secret_access_key=(
                bucket.secret_access_key.get_secret_value() if bucket.secret_access_key else None
            ),
            config=Config(s3={"addressing_style": "path"}),
        )
        return cls(bucket=bucket, client=client)

    def list_keys(self, prefix: str = "") -> list[str]:
        paginator = self.client.get_paginator("list_objects_v2")
        keys: list[str] = []
        for page in paginator.paginate(Bucket=self.bucket.name, Prefix=self.bucket.prefix + prefix):
            keys += [obj["Key"] for obj in page.get("Contents", []) if "Key" in obj]
        return keys

    def read(self, key: str) -> bytes:
        return self.client.get_object(Bucket=self.bucket.name, Key=key)["Body"].read()

    def delete(self, key: str) -> None:
        self.client.delete_object(Bucket=self.bucket.name, Key=key)

    def writable(self) -> bool:
        """Whether a probe object can be written and read back now."""
        key = self.bucket.prefix + ".e2e-probe"
        try:
            self.client.put_object(Bucket=self.bucket.name, Key=key, Body=b"probe")
            return self.read(key) == b"probe"
        except BotoCoreError, ClientError:
            return False

    def exists(self) -> bool:
        self.client.head_bucket(Bucket=self.bucket.name)
        return True
