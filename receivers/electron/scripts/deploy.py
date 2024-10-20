import os
import hashlib
import boto3
from botocore.client import Config
import shutil

ACCOUNT_ID = os.environ.get('R2_ACCOUNT_ID')
ACCESS_KEY_ID = os.environ.get('R2_ACCESS_KEY_ID')
SECRET_ACCESS_KEY = os.environ.get('R2_SECRET_ACCESS_KEY')
BUCKET_NAME = os.environ.get('R2_BUCKET_NAME')

LOCAL_CACHE_DIR = os.environ.get('FCAST_LOCAL_CACHE_DIR')

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
list_response = s3.list_objects_v2(Bucket=BUCKET_NAME)
bucket_files = list_response.get('Contents', [])

def copy_artifacts_to_local_cache():
    print('Copying artifacts to cache...')
    dst = os.path.join(LOCAL_CACHE_DIR, 'temp')
    shutil.copytree('/artifacts', f'{dst}', dirs_exist_ok=True, ignore=shutil.ignore_patterns('*.w*'))

# TODO: do partial sync to prevent downloading full bucket (only what is needed for delta updates and purge old files
def sync_local_cache():
    print('Syncing local cache with s3...')
    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            local_files.append(os.path.relpath(os.path.join(root, filename), LOCAL_CACHE_DIR))

    for obj in bucket_files:
        filename = obj['Key']
        save_path = os.path.join(LOCAL_CACHE_DIR, filename)

        if filename not in local_files:
            print(f'Downloading file: {filename}')
            get_response = s3.get_object(Bucket=BUCKET_NAME, Key=filename)

            os.makedirs(os.path.dirname(save_path), exist_ok=True)
            with open(save_path, 'wb') as file:
                file.write(get_response['Body'].read())

def upload_local_cache():
    print('Uploading local cache to s3...')
    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            local_files.append(os.path.relpath(os.path.join(root, filename), LOCAL_CACHE_DIR))

    for file_path in local_files:
        if file_path not in map(lambda x: x['Key'], bucket_files):
            print(f'Uploading file: {file_path}')

            with open(os.path.join(LOCAL_CACHE_DIR, file_path), 'rb') as file:
                put_response = s3.put_object(
                    Body=file,
                    Bucket=BUCKET_NAME,
                    Key=file_path,
                )

def generate_delta_updates():
    pass

# generate html previous version browsing (based off of bucket + and local if does not have all files)
def generate_previous_releases_page():
    pass

def update_website():
    pass

# CI Operations
copy_artifacts_to_local_cache()
sync_local_cache()
# generate_delta_updates()
upload_local_cache()
# generate_previous_releases_page()
# update_website()
