import boto3
import hashlib
import os
import requests
import shutil
from botocore.client import Config
from collections import namedtuple
from functools import cmp_to_key

CLOUDFLARE_CACHE_TOKEN = os.environ.get('CLOUDFLARE_CACHE_TOKEN')
ZONE_ID = os.environ.get('CLOUDFLARE_ZONE_ID')
ACCOUNT_ID = os.environ.get('R2_ACCOUNT_ID')
ACCESS_KEY_ID = os.environ.get('R2_ACCESS_KEY_ID')
SECRET_ACCESS_KEY = os.environ.get('R2_SECRET_ACCESS_KEY')
BUCKET_NAME = os.environ.get('R2_BUCKET_NAME')

EXCLUDED_BUCKET_FILES = ['electron/releases_v1.json']
class S3Client:
    def __init__(self, cache_version_amount, excluded_delta_versions):
        # Note: Cloudflare R2 docs outdated, secret is not supposed to be hashed...

        # Hash the secret access key using SHA-256
        #hashed_secret_key = hashlib.sha256(SECRET_ACCESS_KEY.encode()).hexdigest()

        # Configure the S3 client for Cloudflare R2
        self.s3 = boto3.client('s3',
            endpoint_url=f'https://{ACCOUNT_ID}.r2.cloudflarestorage.com',
            aws_access_key_id=ACCESS_KEY_ID,
            # aws_secret_access_key=hashed_secret_key,
            aws_secret_access_key=SECRET_ACCESS_KEY,
            config=Config(
                signature_version='s3v4'
            )
        )
        list_response = self.s3.list_objects_v2(Bucket=BUCKET_NAME, Prefix='electron/')
        self.bucket_files = list_response.get('Contents', [])

        bucket_files_versions = filter(lambda x: x['Key'] not in EXCLUDED_BUCKET_FILES, self.bucket_files)
        self.bucket_versions_full = sorted(set(map(lambda x: x['Key'].split('/')[1], bucket_files_versions)), key=cmp_to_key(compare_versions), reverse=True)
        self.bucket_versions = self.bucket_versions_full if cache_version_amount < 0 else self.bucket_versions_full[:cache_version_amount]
        self.bucket_delta_versions = [v for v in self.bucket_versions if v not in excluded_delta_versions]

    def get_bucket_files(self):
        return self.bucket_files

    def get_versions(self, full=False):
        return self.bucket_versions_full if full else self.bucket_versions

    def download_file(self, full_path, s3_path):
        print(f'Downloading file: {s3_path}')
        get_response = self.s3.get_object(Bucket=BUCKET_NAME, Key=s3_path)

        os.makedirs(os.path.dirname(full_path), exist_ok=True)
        with open(full_path, 'wb') as file:
            file.write(get_response['Body'].read())

    def upload_file(self, full_path, s3_path):
        print(f'Uploading file: {s3_path}')

        domain = BUCKET_NAME.replace('-', '.')
        purge_response = requests.post(
            f'https://api.cloudflare.com/client/v4/zones/{ZONE_ID}/purge_cache',
            headers={
                'Authorization': f'Bearer {CLOUDFLARE_CACHE_TOKEN}',
                'Content-Type': 'application/json',
            },
            json={
                'files': [f'https://{domain}/{s3_path}']
            }
        )

        if purge_response.status_code != 200:
            print(f'Error while purging cache: {purge_response}')

        with open(full_path, 'rb') as file:
            put_response = self.s3.put_object(
                Body=file,
                Bucket=BUCKET_NAME,
                Key=s3_path,
            )

# Utility types
class PackageFormat:
    """Parses an artifact path to extract package information

    Artifact format: ((VERSION)?/PKG/(OS/ARCH|ARCH)/)?fcast-receiver-VERSION-OS-ARCH(-setup|-delta-DELTA_BASE_VERSION)?(-CHANNEL-CHANNEL_VERSION)?.PKG
    """

    def __init__(self, path):
        self.version = None
        self.type = None
        self.os = None
        self.os_pretty = None
        self.arch = None
        self.name = None
        self.is_delta = False
        self.delta_base_version = None
        self.channel = None
        self.channel_version = None

        dirs = path.split('/')
        file = path.split('-')
        self.name = 'fcast-receiver'

        if len(dirs) > 1:
            parse_index = 0

            if dirs[parse_index].count('.') > 0:
                self.version = dirs[parse_index]
                self.type = dirs[parse_index + 1]
                parse_index += 2
            else:
                self.type = dirs[parse_index]
                parse_index += 1

            if self.type == 'zip':
                self.os = dirs[parse_index]
                self.os_pretty = 'windows' if self.os == 'win32' else 'macOS' if self.os == 'darwin' else 'linux'
                self.arch = dirs[parse_index + 1]
                parse_index += 2
            else:
                if self.type == 'wix':
                    self.os = 'win32'
                    self.os_pretty = 'windows'
                    self.arch = dirs[parse_index]
                elif self.type == 'dmg':
                    self.os = 'darwin'
                    self.os_pretty = 'macOS'
                    self.arch = dirs[parse_index]
                elif self.type == 'deb' or self.type == 'rpm':
                    self.os = 'linux'
                    self.os_pretty = 'linux'
                    self.arch = dirs[parse_index]
                parse_index += 1

            # Unsupported package format (e.g. 1.0.14)
            if self.version == '1.0.14':
                return

            file = dirs[parse_index].split('-')

        self.version = file[2]
        channel_index = 5
        if len(file) == channel_index:
            self.channel = 'stable'
            return

        if file[channel_index] == 'delta':
            self.is_delta = True
            self.delta_base_version = file[channel_index + 1].replace('.delta', '')
            channel_index += 2
        elif file[channel_index] == 'setup':
            channel_index += 1

        if len(file) > channel_index:
            self.channel = file[channel_index]
            version = file[channel_index + 1]
            self.channel_version = version[:version.rfind('.')]
        else:
            self.channel = 'stable'

    def packageNamePretty(self):
        if self.channel != 'stable':
            return f'{self.name}-{self.version}-{self.os_pretty}-{self.arch}-{self.channel}-{self.channel_version}'
        else:
            return f'{self.name}-{self.version}-{self.os_pretty}-{self.arch}'

    def __str__(self) -> str:
        return f'''PackageFormat(type={self.type}, version={self.version}, os={self.os}, arch={self.arch},
                   is_delta={self.is_delta}, delta_base_version={self.delta_base_version}, channel={self.channel},
                   channel_version={self.channel_version})'''

ArtifactVersion = namedtuple('ArtifactVersion', ['version', 'channel', 'channel_version'])

# Utility functions
def compare_versions(x, y):
    x_parts = x.split('.')
    y_parts = y.split('.')

    for i in range(len(x_parts)):
        if x_parts[i] < y_parts[i]:
            return -1
        elif x_parts[i] > y_parts[i]:
            return 1

    return 0

def generate_update_tarball(full_path, rel_path, working_dir, package):
    if package.os == 'darwin':
        temp_working_dir = os.path.join(working_dir, os.path.dirname(rel_path), f'{package.name}-{package.os}-{package.arch}')
        extract_dir = temp_working_dir
    else:
        temp_working_dir = os.path.join(working_dir, os.path.dirname(rel_path))
        extract_dir = os.path.join(temp_working_dir, f'{package.name}-{package.os}-{package.arch}')

    shutil.unpack_archive(full_path, temp_working_dir)

    if package.os == 'darwin':
        shutil.make_archive(os.path.join(working_dir, os.path.dirname(rel_path), package.packageNamePretty()), 'tar', extract_dir)
        shutil.rmtree(temp_working_dir)

        temp_working_dir = os.path.join(working_dir, os.path.dirname(rel_path))
    else:
        shutil.make_archive(os.path.join(temp_working_dir, package.packageNamePretty()), 'tar', extract_dir)
        shutil.rmtree(extract_dir)

    digest = ''
    artifact_name = f'{package.packageNamePretty()}.tar'
    with open(os.path.join(temp_working_dir, artifact_name), 'rb') as file:
        digest = hashlib.sha256(file.read()).hexdigest()

    return artifact_name, digest
