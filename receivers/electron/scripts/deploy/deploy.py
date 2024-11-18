import os
import hashlib
import json
import shutil
from functools import cmp_to_key
from util import BUCKET_NAME, S3Client, PackageFormat, ArtifactVersion, compare_versions, generate_update_tarball

DEPLOY_DIR = os.environ.get('FCAST_DO_RUNNER_DEPLOY_DIR')
TEMP_DIR = os.path.join(DEPLOY_DIR, 'temp')
LOCAL_CACHE_DIR = os.path.join(DEPLOY_DIR, 'cache')
BASE_DOWNLOAD_URL = BUCKET_NAME.replace('-', '.')
EXCLUDED_DELTA_VERSIONS = ["1.0.14"]

# Version tracking for migration support
RELEASES_JSON_FILE_VERSION = 1
RELEASES_JSON_MAJOR_VERSION = '1'
RELEASES_JSON = f'releases_v{RELEASES_JSON_MAJOR_VERSION}.json'

# Customizable CI parameters
CACHE_VERSION_AMOUNT = int(os.environ.get('CACHE_VERSION_AMOUNT', default="-1"))

s3 = S3Client(CACHE_VERSION_AMOUNT, EXCLUDED_DELTA_VERSIONS)

# CI functions
def ensure_files_exist(dirs, files):
    for d in dirs:
        os.makedirs(d, exist_ok=True)

    for f in files:
        if not os.path.exists(os.path.join(LOCAL_CACHE_DIR, f)):
            s3.download_file(os.path.join(LOCAL_CACHE_DIR, f), f)

def copy_artifacts_to_local_cache():
    version = None
    with open(os.path.join(LOCAL_CACHE_DIR, 'electron', RELEASES_JSON) , 'r') as file:
        releases = json.load(file)
        version = ArtifactVersion(releases['currentVersion'], 'stable', None)

    if len(os.listdir('/artifacts')) == 0:
        print('No artifacts were built...')
        return version

    print('Copying artifacts to cache...')
    # Picking a random package that exists from the build pipeline
    artifact = PackageFormat(os.listdir('/artifacts/zip/linux/x64')[0])
    version = ArtifactVersion(artifact.version, artifact.channel, artifact.channel_version)
    dst = os.path.join(TEMP_DIR, version.version)

    shutil.copytree('/artifacts', dst, dirs_exist_ok=True, ignore=shutil.ignore_patterns('*.w*'))
    for dir in os.listdir('/artifacts'):
        shutil.rmtree(os.path.join('/artifacts', dir))

    print(f'Current app version: {version}')
    return version

def sync_local_cache():
    print('Syncing local cache with s3...')
    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            rel_path = os.path.relpath(os.path.join(root, filename), LOCAL_CACHE_DIR)
            version = os.path.relpath(rel_path, 'electron/').split('/')[0]

            if version in s3.get_versions() or filename == RELEASES_JSON:
                local_files.append(rel_path)
            elif filename != RELEASES_JSON:
                print(f'Purging file from local cache: {rel_path}')
                os.remove(os.path.join(root, filename))

    for obj in s3.get_bucket_files():
        filename = obj['Key']
        path = os.path.join(LOCAL_CACHE_DIR, filename)

        if filename not in local_files:
            s3.download_file(path, filename)

def upload_local_cache():
    print('Uploading local cache to s3...')
    shutil.copytree(TEMP_DIR, os.path.join(LOCAL_CACHE_DIR, 'electron'), dirs_exist_ok=True)

    local_files = []
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in files:
            full_path = os.path.join(root, filename)
            rel_path = os.path.relpath(full_path, LOCAL_CACHE_DIR)
            local_files.append(rel_path)

    for file_path in local_files:
        if file_path not in map(lambda x: x['Key'], s3.get_bucket_files()) or os.path.basename(file_path) == RELEASES_JSON:
            s3.upload_file(os.path.join(LOCAL_CACHE_DIR, file_path), file_path)

# TODO: WIP
def generate_delta_updates(artifact_version):
    delta_info = {}

    releases = None
    with open(os.path.join(LOCAL_CACHE_DIR, 'electron', RELEASES_JSON) , 'r') as file:
        releases = json.load(file)

    # Get sha digest from base version for integrity validation
    print('Generating sha digests from previous updates...')
    for root, _, files in os.walk(LOCAL_CACHE_DIR):
        for filename in filter(lambda f: f.endswith('.zip'),  files):
            full_path = os.path.join(root, filename)
            rel_path = os.path.relpath(full_path, os.path.join(LOCAL_CACHE_DIR, 'electron'))
            package = PackageFormat(rel_path)

            if package.channel != artifact_version.channel or package.version in EXCLUDED_DELTA_VERSIONS:
                continue

            print(f'Generating sha digests from: {full_path}')
            artifact_name, digest = generate_update_tarball(full_path, rel_path, TEMP_DIR, package)
            print(f'Digest Info: {artifact_name} {digest}')

            os_dict = delta_info.get(package.channel, {})
            arch_dict = os_dict.get(package.os, {})
            version_dict = arch_dict.get(package.arch, {})

            delta_entry = {
                'path': os.path.join(TEMP_DIR, os.path.dirname(rel_path), artifact_name),
                'digest': digest,
            }

            version_dict[package.version] = delta_entry
            arch_dict[package.arch] = version_dict
            os_dict[package.os] = arch_dict
            delta_info[package.channel] = os_dict


    # TODO: Add limit on amount of delta patches to create (either fixed number or by memory savings)
    # TODO: Parallelize bsdiff invocation since its single-threaded, provided enough RAM available
    print('Generating delta updates...')
    previous_versions = filter(lambda v: v not in EXCLUDED_DELTA_VERSIONS, releases['previousVersions'])
    for delta_version in previous_versions:
        # Create delta patches
        for root, _, files in os.walk(TEMP_DIR):
            for filename in filter(lambda f: f.endswith('.zip'),  files):
                full_path = os.path.join(root, filename)
                rel_path = os.path.relpath(full_path, TEMP_DIR)
                package = PackageFormat(rel_path)

                if package.version in EXCLUDED_DELTA_VERSIONS:
                    continue

                artifact_name, digest = generate_update_tarball(full_path, rel_path, TEMP_DIR, package)
                base_file = delta_info[package.channel][package.os][package.arch][delta_version]['path']
                new_file = os.path.join(os.path.dirname(full_path), artifact_name)
                delta_file = os.path.join(os.path.dirname(full_path), f'{package.name}-{package.version}-{package.os_pretty}-{package.arch}-delta-{delta_version}.delta')
                command = f'bsdiff {base_file} {new_file} {delta_file}'

                print(f'temp skipping delta generation: {command}')
                # print(f'Generating delta update: {command}')
                # os.system(command)
                # os.remove(base_file)
                # os.remove(new_file)

    return delta_info

def generate_releases_json(artifact_version, delta_info):
    print(f'Generating {RELEASES_JSON}...')
    releases = None
    with open(os.path.join(LOCAL_CACHE_DIR, 'electron', RELEASES_JSON) , 'r') as file:
        releases = json.load(file)

    current_version = releases.get('currentVersion', '0.0.0')
    current_releases = releases.get('currentReleases', {})
    channel_current_versions = releases.get('channelCurrentVersions', {})

    all_versions = releases.get('allVersions', [])
    if current_version not in all_versions:
        all_versions.append(current_version)

    if compare_versions(artifact_version.version, current_version) < 0 or \
        (artifact_version.channel != 'stable' and int(artifact_version.channel_version) < int(channel_current_versions[artifact_version.channel])):
        print('Uploading older release, skipping release json generation...')
        return

    for root, _, files in os.walk(TEMP_DIR):
        # Only offer zip and delta updates. Other packages will update from zip packages
        for filename in filter(lambda f: f.endswith('.zip') or f.endswith('.delta'),  files):
            full_path = os.path.join(root, filename)
            rel_path = os.path.relpath(full_path, TEMP_DIR)
            package = PackageFormat(rel_path)
            url = f'https://{BASE_DOWNLOAD_URL}/electron/{rel_path}'

            digest = ''
            with open(full_path, 'rb') as file:
                digest = hashlib.sha256(file.read()).hexdigest()

            os_dict = current_releases.get(package.channel, {})
            arch_dict = os_dict.get(package.os, {})
            entry_dict = arch_dict.get(package.arch, {})

            if package.is_delta:
                delta_dict = entry_dict.get('deltas', {})
                delta_entry = {
                    'deltaUrl': url,
                    'sha256Digest': digest,
                    'baseVersion': package.delta_base_version,
                    'baseSha256Digest': delta_info[package.channel][package.os][package.arch][package.delta_base_version]['digest'],
                }
                delta_dict[package.delta_base_version] = delta_entry
                entry_dict['deltas'] = delta_dict
            else:
                entry_dict['url'] = url
                entry_dict['sha256Digest'] = digest

            arch_dict[package.arch] = entry_dict
            os_dict[package.os] = arch_dict
            current_releases[package.channel] = os_dict

            if package.channel != 'stable':
                channel_current_versions[package.channel] = max(int(package.channel_version), channel_current_versions.get(package.channel, 0))

    if artifact_version.channel == 'stable' and max([artifact_version.version, current_version], key=cmp_to_key(compare_versions)):
        releases['currentVersion'] = artifact_version.version
    else:
        releases['currentVersion'] = current_version

    releases['previousVersions'] = s3.get_versions(full=True)
    releases['fileVersion'] = RELEASES_JSON_FILE_VERSION
    releases['allVersions'] = all_versions
    releases['channelCurrentVersions'] = channel_current_versions
    releases['currentReleases'] = current_releases

    with open(os.path.join(LOCAL_CACHE_DIR, 'electron', RELEASES_JSON) , 'w') as file:
        json.dump(releases, file, indent=4)

def generate_previous_releases_page():
    pass

def update_website():
    pass

# CI Operations
ensure_files_exist(dirs=[
    '/artifacts',
    DEPLOY_DIR,
    TEMP_DIR,
    LOCAL_CACHE_DIR,
    os.path.join(LOCAL_CACHE_DIR, 'electron')
],
files=[
    os.path.join('electron', RELEASES_JSON)
])
artifact_version = copy_artifacts_to_local_cache()
sync_local_cache()

# Disabling delta update generation for now...
# delta_info = generate_delta_updates(artifact_version)
delta_info = {}

generate_releases_json(artifact_version, delta_info)
upload_local_cache()
# generate_previous_releases_page()
# update_website()

print('Cleaning up...')
shutil.rmtree(TEMP_DIR)
