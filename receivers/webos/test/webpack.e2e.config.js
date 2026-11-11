const path = require('path');

// Run from the service folder: `npm run build:e2e` in receivers/webos/fcast-receiver-service.
const serviceDir = path.resolve(__dirname, '../fcast-receiver-service');
const commonWebDir = path.resolve(__dirname, '../../common/web');
const serviceModules = path.resolve(serviceDir, 'node_modules');

// webpack is installed in the service folder, not next to this config.
const webpack = require(require.resolve('webpack', { paths: [serviceDir] }));

module.exports = {
    context: __dirname,
    mode: 'development',
    devtool: false,
    target: 'node12',
    entry: './e2e-v4/index.ts',
    // The harness is loaded by absolute path at runtime, so __dirname must be the real bundle folder.
    node: {
        __dirname: false,
        __filename: false,
    },
    module: {
        rules: [
            {
                test: /\.tsx?$/,
                include: [__dirname, commonWebDir],
                use: [
                    {
                        loader: 'ts-loader',
                        options: {
                            transpileOnly: true,
                            configFile: path.resolve(serviceDir, 'tsconfig.json'),
                        },
                    },
                ],
            },
        ],
    },
    resolve: {
        alias: {
            'common': commonWebDir,
        },
        extensions: ['.tsx', '.ts', '.js'],
        extensionAlias: { '.js': ['.ts', '.js'] },
        modules: [serviceModules, 'node_modules'],
    },
    resolveLoader: {
        modules: [serviceModules, 'node_modules'],
    },
    output: {
        filename: 'e2e-v4.js',
        path: path.resolve(__dirname, 'dist'),
    },
    plugins: [
        new webpack.DefinePlugin({
            TARGET: JSON.stringify('webOS'),
        }),
    ],
};
