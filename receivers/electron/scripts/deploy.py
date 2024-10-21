import os
import hashlib
import boto3
from botocore.client import Config
import shutil
from functools import cmp_to_key

ACCOUNT_ID = os.environ.get('R2_ACCOUNT_ID')
ACCESS_KEY_ID = os.environ.get('R2_ACCESS_KEY_ID')
SECRET_ACCESS_KEY = os.environ.get('R2_SECRET_ACCESS_KEY')
BUCKET_NAME = os.environ.get('R2_BUCKET_NAME')

DEPLOY_DIR = os.environ.get('FCAST_DO_RUNNER_DEPLOY_DIR')
TEMP_DIR = os.path.join(DEPLOY_DIR, 'temp')
LOCAL_CACHE_DIR = os.path.join(DEPLOY_DIR, 'cache')

# Customizable CI parameters
CACHE_VERSION_AMOUNT = int(os.environ.get('CACHE_VERSION_AMOUNT', default="-1"))
RELEASE_CANDIDATE = bool(os.environ.get('RELEASE_CANDIDATE', default=False))
RELEASE_CANDIDATE_VERSION = int(os.environ.get('RELEASE_CANDIDATE_VERSION', default="1"))

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

# Initial setup

# Note: Cloudflare R2 docs outdated, secret is not supposed to be hashed...

# Hash the secret access key using SHA-256
#hashed_secret_key = hashlib.sha256(SECRET_ACCESS_KEY.encode()).hexdigest()

# Configure the S3 client for Cloudflare R2
s3 = boto3.client('s3',
    endpoint_url=f'https://{ACCOUNT_ID}.r2.cloudflarestorage.com',
    aws_access_key_id=ACCESS_KEY_ID,
    # aws_secret_access_key=hashed_secret_key,
    aws_secret_access_key=SECRET_ACCESS_KEY,
    config=Config(
        signature_version='s3v4'
    )
)
list_response = s3.list_objects_v2(Bucket=BUCKET_NAME, Prefix='electron/')
bucket_files = list_response.get('Contents', [])
bucket_versions_full = sorted(set(map(lambda x: x['Key'].split('/')[1], bucket_files)), key=cmp_to_key(compare_versions), reverse=True)
bucket_versions = bucket_versions_full if CACHE_VERSION_AMOUNT < 0 else bucket_versions_full[:CACHE_VERSION_AMOUNT]
os.makedirs(TEMP_DIR, exist_ok=True)

# CI functions

def copy_artifacts_to_local_cache():
    if len(os.listdir('/artifacts')) == 0:
        print('No artifacts were built...')
        return None

    print('Copying artifacts to cache...')
    # All artifact should have same version in format: /artifacts/PKG/OS/ARCH/fcast-receiver-VERSION-OS-ARCH.PKG
    version = os.listdir('/artifacts/zip/linux/x64')[0].split('-')[2]
    dst = os.path.join(TEMP_DIR, version)
    print(f'Current app version: {version}')

    shutil.copytree('/artifacts', dst, dirs_exist_ok=True, ignore=shutil.ignore_patterns('*.w*'))
    for dir in os.listdir('/artifacts'):
        shutil.rmtree(os.path.join('/artifacts', dir))

    return version

def sync_local_cache():
    print('Syncing local cache with s3...')
    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            rel_path = os.path.relpath(os.path.join(root, filename), LOCAL_CACHE_DIR)
            version = os.path.relpath(rel_path, 'electron/').split('/')[0]

            if version in bucket_versions:
                local_files.append(rel_path)
            else:
                print(f'Purging file from local cache: {rel_path}')
                os.remove(os.path.join(root, filename))

    for obj in bucket_files:
        filename = obj['Key']
        save_path = os.path.join(LOCAL_CACHE_DIR, filename)

        if filename not in local_files:
            print(f'Downloading file: {filename}')
            get_response = s3.get_object(Bucket=BUCKET_NAME, Key=filename)

            os.makedirs(os.path.dirname(save_path), exist_ok=True)
            with open(save_path, 'wb') as file:
                file.write(get_response['Body'].read())

def upload_local_cache(current_version):
    print('Uploading local cache to s3...')
    shutil.copytree(TEMP_DIR, os.path.join(LOCAL_CACHE_DIR, 'electron'), dirs_exist_ok=True)

    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            full_path = os.path.join(root, filename)
            rel_path = os.path.relpath(full_path, LOCAL_CACHE_DIR)
            version = rel_path.split('/')[1]

            if RELEASE_CANDIDATE and version == current_version:
                rc_path = full_path[:full_path.rfind('.')] + f'-rc{RELEASE_CANDIDATE_VERSION}' + full_path[full_path.rfind('.'):]
                os.rename(full_path, rc_path)
                rel_path = os.path.relpath(rc_path, LOCAL_CACHE_DIR)

            local_files.append(rel_path)

    for file_path in local_files:
        if file_path not in map(lambda x: x['Key'], bucket_files):
            print(f'Uploading file: {file_path}')

            with open(os.path.join(LOCAL_CACHE_DIR, file_path), 'rb') as file:
                put_response = s3.put_object(
                    Body=file,
                    Bucket=BUCKET_NAME,
                    Key=file_path,
                )

def generate_delta_updates(current_version):
    pass

# generate html previous version browsing (based off of bucket + and local if does not have all files)
def generate_previous_releases_page():
    pass

def update_website():
    pass

# CI Operations
current_version = copy_artifacts_to_local_cache()
sync_local_cache()
# generate_delta_updates(current_version)
upload_local_cache(current_version)
# generate_previous_releases_page()
# update_website()

shutil.rmtree(TEMP_DIR)
